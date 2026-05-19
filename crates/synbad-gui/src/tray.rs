//! System tray integration.
//!
//! Enabled with `--features tray`. The default build skips tray support
//! because it pulls in GTK + libappindicator (Linux), which not every dev
//! box has installed.
//!
//! ## Architecture
//!
//! eframe drives a winit event loop, but `tray-icon` on Linux requires a
//! GTK event loop to deliver tray clicks. We resolve this by spawning a
//! dedicated thread that calls `gtk::init()` + `gtk::main()`, owns the
//! `TrayIcon`, and lets `tray-icon` route events into a channel we own.
//!
//! On Windows / macOS the tray icon lives on the main thread (eframe's
//! winit event loop pumps the necessary platform events).
//!
//! ## Waking egui when the window is hidden
//!
//! When the user closes the window to tray, eframe parks the event loop
//! until a wakeup. Tray clicks arrive on a background thread (GTK on
//! Linux, the OS thread on macOS/Windows) and would otherwise pile up
//! in their channel with nobody draining them — neither Show nor Quit
//! would ever take effect.
//!
//! [`set_repaint`] wires `egui::Context::request_repaint` into the
//! [`MenuEvent`] handler. Every tray click now nudges the egui loop, so
//! [`try_recv_menu_id`] inside `update()` actually runs and the command
//! (Visible/Close) is dispatched.
//!
//! Menu items are keyed by stable string IDs so the GUI can match them
//! without sharing a `MenuItem` handle across threads.

use std::sync::{Arc, Mutex, OnceLock};

/// Menu-item ID for the "Show Window" action.
pub const MENU_ID_SHOW: &str = "synbad.show";
/// Menu-item ID for the "Quit" action.
pub const MENU_ID_QUIT: &str = "synbad.quit";

pub type RepaintFn = Arc<dyn Fn() + Send + Sync>;

/// Slot for the egui-repaint callback. Populated from main after eframe
/// hands us the `egui::Context`; the tray handler reads through this
/// to wake the UI loop even if the window is hidden in close-to-tray.
fn repaint_slot() -> &'static Mutex<Option<RepaintFn>> {
    static SLOT: OnceLock<Mutex<Option<RepaintFn>>> = OnceLock::new();
    SLOT.get_or_init(|| Mutex::new(None))
}

/// Channel carrying tray-menu IDs. Set up once at first access so both
/// the install-side handler and `try_recv_menu_id` see the same channel.
type MenuChannel = (
    crossbeam_channel::Sender<String>,
    crossbeam_channel::Receiver<String>,
);
fn menu_channel() -> &'static MenuChannel {
    static CHAN: OnceLock<MenuChannel> = OnceLock::new();
    CHAN.get_or_init(crossbeam_channel::unbounded)
}

/// Install the egui repaint callback used to wake the UI when a tray
/// menu item is clicked. Must be called once `eframe` has handed us a
/// usable [`egui::Context`] (i.e. from inside the app-creator closure).
pub fn set_repaint(repaint: RepaintFn) {
    if let Ok(mut g) = repaint_slot().lock() {
        *g = Some(repaint);
    }
}

/// Drain one queued tray menu-item ID, if any. Non-blocking. Returns
/// strings matching [`MENU_ID_SHOW`] / [`MENU_ID_QUIT`].
pub fn try_recv_menu_id() -> Option<String> {
    menu_channel().1.try_recv().ok()
}

#[cfg(feature = "tray")]
mod imp {
    use super::*;

    use tray_icon::menu::{Menu, MenuEvent, MenuItem, PredefinedMenuItem};
    #[cfg(not(target_os = "linux"))]
    use tray_icon::TrayIcon;
    use tray_icon::{Icon, TrayIconBuilder};

    /// Handle returned to keep the tray alive on Windows / macOS. On Linux
    /// the tray lives entirely on the GTK thread and this struct is empty.
    pub struct TrayHandle {
        #[cfg(not(target_os = "linux"))]
        _icon: TrayIcon,
    }

    fn build_menu() -> Menu {
        let menu = Menu::new();
        let show = MenuItem::with_id(MENU_ID_SHOW, "Show Synbad", true, None);
        let quit = MenuItem::with_id(MENU_ID_QUIT, "Quit", true, None);
        // Ignore errors — the only failure mode here is OS-level menu setup
        // and we'd rather have a partially-populated menu than panic at boot.
        let _ = menu.append(&show);
        let _ = menu.append(&PredefinedMenuItem::separator());
        let _ = menu.append(&quit);
        menu
    }

    fn build_icon() -> Icon {
        // Tiny 16x16 solid-color RGBA so we don't ship a binary asset. The
        // real artwork can replace this later.
        const W: u32 = 16;
        const H: u32 = 16;
        let mut rgba = Vec::with_capacity((W * H * 4) as usize);
        for y in 0..H {
            for x in 0..W {
                // Two diagonal bands so the icon is recognizably non-blank.
                let on = ((x + y) / 4) % 2 == 0;
                if on {
                    rgba.extend_from_slice(&[0x4f, 0x9c, 0xff, 0xff]);
                } else {
                    rgba.extend_from_slice(&[0x12, 0x29, 0x4d, 0xff]);
                }
            }
        }
        Icon::from_rgba(rgba, W, H).expect("16x16 RGBA must be a valid icon")
    }

    /// Install a global menu-event handler that forwards each click's ID
    /// into our channel and pokes the egui loop so it wakes up to process
    /// it. Replaces muda's default channel-only delivery — without the
    /// repaint nudge, clicks arriving while the window is hidden would
    /// sit in the channel forever.
    fn install_event_handler() {
        let tx = super::menu_channel().0.clone();
        MenuEvent::set_event_handler(Some(move |ev: MenuEvent| {
            // `id().as_ref()` projects &MenuId down to &str.
            let id = ev.id.as_ref().to_string();
            let _ = tx.send(id);
            if let Ok(slot) = super::repaint_slot().lock() {
                if let Some(repaint) = slot.as_ref() {
                    repaint();
                }
            }
        }));
    }

    /// Install the tray. Returns a handle that must be kept alive for the
    /// lifetime of the app (on Linux the tray lives on its own thread and
    /// the handle is a no-op; on Windows / macOS it owns the `TrayIcon`).
    ///
    /// Failures here are intentionally non-fatal — the app still works
    /// without a tray, so we log and continue.
    pub fn install() -> Option<TrayHandle> {
        // Register the menu-event bridge before the tray exists so the
        // very first click is captured. set_event_handler is safe to call
        // before any UI is up — it just rewires muda's static dispatch.
        install_event_handler();

        #[cfg(target_os = "linux")]
        {
            // Channel so we can surface init failures back to the caller.
            let (ready_tx, ready_rx) = std::sync::mpsc::channel::<Result<(), String>>();
            std::thread::Builder::new()
                .name("synbad-tray-gtk".into())
                .spawn(move || {
                    if let Err(e) = gtk::init() {
                        let _ = ready_tx.send(Err(format!("gtk::init failed: {e}")));
                        return;
                    }
                    let menu = build_menu();
                    let icon = build_icon();
                    let built = TrayIconBuilder::new()
                        .with_menu(Box::new(menu))
                        .with_icon(icon)
                        .with_tooltip("Synbad")
                        .build();
                    match built {
                        Ok(_tray) => {
                            let _ = ready_tx.send(Ok(()));
                            // `_tray` must outlive gtk::main; both live until the
                            // process exits because we never return.
                            gtk::main();
                        }
                        Err(e) => {
                            let _ = ready_tx.send(Err(format!("tray build failed: {e}")));
                        }
                    }
                })
                .ok()?;

            match ready_rx.recv_timeout(std::time::Duration::from_secs(2)) {
                Ok(Ok(())) => Some(TrayHandle {}),
                Ok(Err(e)) => {
                    tracing::warn!("tray disabled: {e}");
                    None
                }
                Err(_) => {
                    tracing::warn!("tray init timed out");
                    None
                }
            }
        }
        #[cfg(not(target_os = "linux"))]
        {
            match TrayIconBuilder::new()
                .with_menu(Box::new(build_menu()))
                .with_icon(build_icon())
                .with_tooltip("Synbad")
                .build()
            {
                Ok(icon) => Some(TrayHandle { _icon: icon }),
                Err(e) => {
                    tracing::warn!("tray disabled: {e}");
                    None
                }
            }
        }
    }
}

#[cfg(not(feature = "tray"))]
mod imp {
    /// Empty stand-in so the rest of the code doesn't need cfg-gating.
    pub struct TrayHandle;

    pub fn install() -> Option<TrayHandle> {
        None
    }
}

// Re-export so external code can refer to `tray::install()` etc. The handle
// is only named transitively (via Option<_> bindings), so silence the
// otherwise-noisy unused-import warning.
#[allow(unused_imports)]
pub use imp::{install, TrayHandle};
