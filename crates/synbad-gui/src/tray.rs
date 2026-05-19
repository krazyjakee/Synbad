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
//! `TrayIcon`, and lets `tray-icon` route events into the global
//! `MenuEvent` channel. The main thread polls
//! [`menu_event_receiver`] from inside the egui frame loop.
//!
//! On Windows / macOS the tray icon lives on the main thread (eframe's
//! winit event loop pumps the necessary platform events).
//!
//! Menu items are keyed by stable string IDs so the GUI can match them
//! without sharing a `MenuItem` handle across threads.

/// Menu-item ID for the "Show Window" action.
pub const MENU_ID_SHOW: &str = "synbad.show";
/// Menu-item ID for the "Quit" action.
pub const MENU_ID_QUIT: &str = "synbad.quit";

#[cfg(feature = "tray")]
mod imp {
    use super::*;

    use tray_icon::menu::{Menu, MenuEvent, MenuEventReceiver, MenuItem, PredefinedMenuItem};
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

    /// Install the tray. Returns a handle that must be kept alive for the
    /// lifetime of the app (on Linux the tray lives on its own thread and
    /// the handle is a no-op; on Windows / macOS it owns the `TrayIcon`).
    ///
    /// Failures here are intentionally non-fatal — the app still works
    /// without a tray, so we log and continue.
    pub fn install() -> Option<TrayHandle> {
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

    /// Borrow the global menu-event receiver. Each emitted `MenuEvent` has
    /// `event.id.0` matching one of our `MENU_ID_*` constants.
    pub fn menu_event_receiver() -> &'static MenuEventReceiver {
        MenuEvent::receiver()
    }

    /// Re-export so callers don't need a direct `tray_icon` dep.
    pub use tray_icon::menu::MenuEvent as TrayMenuEvent;
}

#[cfg(not(feature = "tray"))]
mod imp {
    /// Empty stand-in so the rest of the code doesn't need cfg-gating.
    pub struct TrayHandle;

    pub fn install() -> Option<TrayHandle> {
        None
    }

    /// A receiver that never delivers anything — keeps the polling code
    /// at the call site uniform whether or not the feature is enabled.
    pub struct NoopRecv;
    impl NoopRecv {
        pub fn try_recv(&self) -> Result<TrayMenuEvent, ()> {
            Err(())
        }
    }
    pub fn menu_event_receiver() -> NoopRecv {
        NoopRecv
    }

    /// Stand-in event type whose `id.as_ref()` always fails to match.
    pub struct TrayMenuEvent;
    impl TrayMenuEvent {
        pub fn id(&self) -> &str {
            ""
        }
    }
}

// Re-export so external code can refer to `tray::install()` etc. The handle
// and event type are only named transitively (via Option<_> bindings), so
// silence the otherwise-noisy unused-import warning.
#[allow(unused_imports)]
pub use imp::{install, menu_event_receiver, TrayHandle, TrayMenuEvent};
