//! Per-user single-instance guard for the GUI process.
//!
//! Without this guard, every launch (clicking the launcher icon, autostart
//! firing, `cargo run` while one is already up) spawns a fresh `synbad-gui`
//! process. Each process installs its own tray icon, and the second tray
//! icon's Quit only kills its own process — the original stays running,
//! and the user sees "Quit doesn't work" because the icon they expected
//! to disappear is owned by the other process.
//!
//! The guard binds a Unix socket at a stable per-user path. If the bind
//! succeeds, we own the lock; a background thread listens for `SHOW`
//! pings from later launches and raises the existing window. If the bind
//! fails with `AddrInUse` and connecting to that socket succeeds, another
//! instance is alive — we forward the SHOW and exit. A stale socket
//! (bind fails, connect fails) is removed and retried.
//!
//! On Windows this module is a no-op stub: the user is on Linux/macOS
//! today, and a named-pipe equivalent can be added when needed.

use std::path::PathBuf;

/// Outcome of [`acquire`].
pub enum AcquireResult {
    /// We're the only running instance. The guard must outlive the GUI —
    /// dropping it removes the socket.
    // Constructed only by the Unix `acquire`; the Windows stub returns
    // `Unsupported`, so the variants look unused under `#[cfg(windows)]`.
    #[cfg_attr(not(unix), allow(dead_code))]
    Acquired(Guard, crossbeam_channel::Receiver<()>),
    /// Another instance is already running and we forwarded a SHOW ping
    /// to it. The caller should exit cleanly.
    #[cfg_attr(not(unix), allow(dead_code))]
    Forwarded,
    /// Single-instance enforcement isn't available on this platform or
    /// the lock socket couldn't be set up. Caller proceeds as if there
    /// were no guard (the previous behaviour).
    Unsupported,
}

pub struct Guard {
    #[cfg(unix)]
    socket_path: PathBuf,
}

#[cfg(unix)]
impl Drop for Guard {
    fn drop(&mut self) {
        // Best-effort cleanup. If we crash without dropping, the next
        // launch will find the socket, fail to connect, and reclaim it.
        let _ = std::fs::remove_file(&self.socket_path);
    }
}

#[cfg(unix)]
pub fn acquire(
    socket_path: PathBuf,
    repaint: std::sync::Arc<dyn Fn() + Send + Sync>,
) -> AcquireResult {
    use std::io::Write;
    use std::os::unix::net::{UnixListener, UnixStream};

    if let Some(parent) = socket_path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }

    // We try at most twice: if the first bind reports AddrInUse and the
    // existing socket turns out to be stale (no one is accept()ing), we
    // remove the file and try again. A second AddrInUse means a real race
    // lost to another launcher — treat it as forwarded.
    for attempt in 0..2 {
        match UnixListener::bind(&socket_path) {
            Ok(listener) => {
                let (tx, rx) = crossbeam_channel::unbounded::<()>();
                let path_clone = socket_path.clone();
                std::thread::Builder::new()
                    .name("synbad-instance".into())
                    .spawn(move || listen(listener, tx, repaint))
                    .expect("spawn single-instance thread");
                return AcquireResult::Acquired(
                    Guard {
                        socket_path: path_clone,
                    },
                    rx,
                );
            }
            Err(e) if e.kind() == std::io::ErrorKind::AddrInUse => {
                // Probe the existing socket. A live peer accept()s and
                // we send SHOW; a stale socket file (previous instance
                // crashed without cleanup) refuses the connection and
                // we reclaim it.
                match UnixStream::connect(&socket_path) {
                    Ok(mut s) => {
                        let _ = s.write_all(b"SHOW\n");
                        let _ = s.flush();
                        return AcquireResult::Forwarded;
                    }
                    Err(_) if attempt == 0 => {
                        tracing::info!(?socket_path, "removing stale single-instance socket");
                        let _ = std::fs::remove_file(&socket_path);
                        continue;
                    }
                    Err(e) => {
                        tracing::warn!(?e, "could not contact existing instance");
                        return AcquireResult::Unsupported;
                    }
                }
            }
            Err(e) => {
                tracing::warn!(?e, ?socket_path, "single-instance bind failed");
                return AcquireResult::Unsupported;
            }
        }
    }
    AcquireResult::Unsupported
}

#[cfg(unix)]
fn listen(
    listener: std::os::unix::net::UnixListener,
    show_tx: crossbeam_channel::Sender<()>,
    repaint: std::sync::Arc<dyn Fn() + Send + Sync>,
) {
    use std::io::Read;
    for stream in listener.incoming() {
        let Ok(mut s) = stream else { continue };
        let mut buf = [0u8; 16];
        let n = s.read(&mut buf).unwrap_or(0);
        if n >= 4 && &buf[..4] == b"SHOW" {
            let _ = show_tx.send(());
            repaint();
        }
    }
}

#[cfg(not(unix))]
pub fn acquire(
    _socket_path: PathBuf,
    _repaint: std::sync::Arc<dyn Fn() + Send + Sync>,
) -> AcquireResult {
    // Windows path: a named-pipe equivalent isn't wired up yet. Returning
    // Unsupported preserves the pre-existing "no guard" behaviour, which
    // matches what users on Windows have today.
    AcquireResult::Unsupported
}

/// Default path for the GUI's single-instance socket. Per-user via
/// `state_dir`, so concurrent users on a shared machine don't collide.
pub fn default_socket_path() -> PathBuf {
    synbad_config::paths::state_dir().join("synbad-gui.sock")
}
