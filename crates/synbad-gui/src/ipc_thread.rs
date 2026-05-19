//! Background threads that talk to `synbadd` over the local socket.
//!
//! There are two roles:
//! 1. A **reader thread** owns a persistent `Subscribe`d connection and
//!    pumps events out to the UI.
//! 2. A **dispatcher thread** consumes `Cmd`s from the UI and issues each
//!    one on a short-lived connection (avoids interleaving with the
//!    streaming subscription).
//!
//! Both reconnect on their own if the daemon goes away.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

#[cfg(unix)]
use std::os::unix::process::CommandExt;

use crossbeam_channel::{Receiver, Sender};

use synbad_config::{paths, Config};
use synbad_ipc::client::Connection;
use synbad_ipc::{
    DaemonState, DiscoveredPeer, Event, Message, Request, Response, TrustedPeer,
};

#[derive(Debug, Clone)]
pub enum Cmd {
    Start,
    Stop,
    Restart,
    SetConfig(Config),
    Refresh,
    /// Pull the latest peer list snapshot from the daemon.
    RefreshPeers,
    /// Initiate a pairing session with the named discovered peer.
    StartPairing { machine_id: String },
    /// Reply to a `PairingProposed` event with the user's verdict.
    ConfirmPairing { session_id: String, accept: bool },
    /// Forget a previously-paired peer.
    RevokeTrust { machine_id: String },
}

#[derive(Debug, Clone)]
pub enum Update {
    Connected,
    Disconnected(String),
    Status { state: DaemonState, recent_log: Vec<String> },
    Config(Config),
    Log(String),
    StateChanged(DaemonState),
    Error(String),
    PeerConnected(String),
    PeerDisconnected(String),
    ActiveScreen(String),
    /// A peer was discovered or refreshed via mDNS.
    PeerDiscovered(DiscoveredPeer),
    /// A peer is no longer visible.
    PeerLost(String),
    /// Replacement snapshot of the daemon's peer list (e.g. on bootstrap).
    PeerSnapshot(Vec<DiscoveredPeer>),
    /// The local machine's stable identity (UUID + fingerprint), used to
    /// render "this is us" in the GUI.
    LocalIdentity { machine_id: String, fingerprint: String },
    /// A pairing handshake reached the user-confirmation step.
    PairingProposed {
        session_id: String,
        peer_machine_id: String,
        peer_display_name: String,
        peer_fingerprint: String,
        verification_code: String,
    },
    /// A pairing handshake completed; peer is now trusted.
    PairingCompleted(TrustedPeer),
    /// A pairing handshake failed.
    PairingFailed { session_id: String, reason: String },
    /// Replacement snapshot of the trusted-peer set.
    TrustedSnapshot(Vec<TrustedPeer>),
    /// A peer's trust was revoked.
    TrustRevoked(String),
}

pub struct IpcHandle {
    pub cmd_tx: Sender<Cmd>,
    pub update_rx: Receiver<Update>,
}

pub fn spawn(socket_path: PathBuf, repaint: Arc<dyn Fn() + Send + Sync>) -> IpcHandle {
    let (cmd_tx, cmd_rx) = crossbeam_channel::unbounded::<Cmd>();
    let (update_tx, update_rx) = crossbeam_channel::unbounded::<Update>();

    {
        let socket_path = socket_path.clone();
        let update_tx = update_tx.clone();
        let repaint = repaint.clone();
        thread::Builder::new()
            .name("synbad-ipc-events".into())
            .spawn(move || event_loop(socket_path, update_tx, repaint))
            .expect("spawn event thread");
    }
    {
        let socket_path = socket_path.clone();
        let update_tx = update_tx.clone();
        let repaint = repaint.clone();
        thread::Builder::new()
            .name("synbad-ipc-cmds".into())
            .spawn(move || command_loop(socket_path, cmd_rx, update_tx, repaint))
            .expect("spawn cmd thread");
    }

    IpcHandle { cmd_tx, update_rx }
}

fn event_loop(
    socket_path: PathBuf,
    update_tx: Sender<Update>,
    repaint: Arc<dyn Fn() + Send + Sync>,
) {
    let mut backoff = Duration::from_millis(500);
    let mut last_spawn: Option<Instant> = None;
    loop {
        match Connection::connect(&socket_path) {
            Ok(mut conn) => {
                backoff = Duration::from_millis(500);
                let _ = update_tx.send(Update::Connected);
                repaint();

                if let Err(e) = bootstrap(&mut conn, &update_tx) {
                    let _ = update_tx.send(Update::Disconnected(e));
                    repaint();
                    thread::sleep(backoff);
                    continue;
                }

                if let Err(e) = conn.send(Request::Subscribe) {
                    let _ = update_tx.send(Update::Disconnected(format!("subscribe: {}", e)));
                    repaint();
                    continue;
                }
                // Drain the Subscribe ack.
                if let Err(e) = conn.recv() {
                    let _ = update_tx.send(Update::Disconnected(e.to_string()));
                    repaint();
                    continue;
                }
                if let Err(e) = conn.make_blocking() {
                    let _ = update_tx.send(Update::Error(e.to_string()));
                }

                // Stream events until the connection breaks.
                loop {
                    match conn.recv() {
                        Ok(Message::Event(ev)) => {
                            let upd = match ev {
                                Event::Log { line } => Update::Log(line),
                                Event::State { state } => Update::StateChanged(state),
                                Event::ConfigChanged => match refetch_config(&socket_path) {
                                    Some(c) => Update::Config(c),
                                    None => continue,
                                },
                                Event::PeerConnected { name } => Update::PeerConnected(name),
                                Event::PeerDisconnected { name } => {
                                    Update::PeerDisconnected(name)
                                }
                                Event::ActiveScreen { name } => Update::ActiveScreen(name),
                                Event::PeerDiscovered { peer } => Update::PeerDiscovered(peer),
                                Event::PeerLost { machine_id } => Update::PeerLost(machine_id),
                                Event::PairingProposed {
                                    session_id,
                                    peer_machine_id,
                                    peer_display_name,
                                    peer_fingerprint,
                                    verification_code,
                                } => Update::PairingProposed {
                                    session_id,
                                    peer_machine_id,
                                    peer_display_name,
                                    peer_fingerprint,
                                    verification_code,
                                },
                                Event::PairingCompleted { peer } => Update::PairingCompleted(peer),
                                Event::PairingFailed { session_id, reason } => {
                                    Update::PairingFailed { session_id, reason }
                                }
                                Event::TrustRevoked { machine_id } => {
                                    Update::TrustRevoked(machine_id)
                                }
                                // Config-sync events surface in the log
                                // pane for now. A future GUI iteration
                                // can render dedicated chips per peer.
                                Event::SyncStarted { peer_machine_id, direction } => {
                                    Update::Log(format!(
                                        "[sync] {:?} session opened with {}",
                                        direction, peer_machine_id
                                    ))
                                }
                                Event::SyncCompleted {
                                    peer_machine_id,
                                    direction,
                                    updated,
                                    new_head,
                                } => Update::Log(format!(
                                    "[sync] {:?} with {} {} (head {})",
                                    direction,
                                    peer_machine_id,
                                    if updated { "merged updates" } else { "no-op" },
                                    new_head
                                )),
                                Event::SyncFailed { peer_machine_id, direction, reason } => {
                                    Update::Log(format!(
                                        "[sync] {:?} with {} failed: {}",
                                        direction, peer_machine_id, reason
                                    ))
                                }
                            };
                            let _ = update_tx.send(upd);
                            repaint();
                        }
                        Ok(_) => {}
                        Err(e) => {
                            let _ = update_tx.send(Update::Disconnected(e.to_string()));
                            repaint();
                            break;
                        }
                    }
                }
            }
            Err(e) => {
                let _ = update_tx.send(Update::Disconnected(format!(
                    "could not connect to synbadd at {:?}: {}",
                    socket_path, e
                )));
                repaint();

                // Auto-launch the daemon if nothing is listening. Throttled
                // so a daemon that crashes on startup doesn't get re-spawned
                // every backoff tick (which would also accumulate zombies).
                let should_spawn = last_spawn
                    .map(|t| t.elapsed() >= Duration::from_secs(3))
                    .unwrap_or(true);
                if should_spawn {
                    last_spawn = Some(Instant::now());
                    match spawn_daemon() {
                        Ok(child) => tracing::info!(
                            pid = child.id(),
                            "no daemon reachable — spawned synbadd"
                        ),
                        Err(e) => tracing::warn!(?e, "failed to spawn synbadd"),
                    }
                }
            }
        }

        thread::sleep(backoff);
        backoff = (backoff * 2).min(Duration::from_secs(10));
    }
}

/// Locate the `synbadd` executable: prefer one alongside our own binary
/// (typical for both `cargo build` layouts and installed bundles), and fall
/// back to PATH lookup so a system-installed daemon still works.
fn synbadd_binary() -> PathBuf {
    let name = if cfg!(windows) { "synbadd.exe" } else { "synbadd" };
    if let Ok(self_exe) = std::env::current_exe() {
        if let Some(dir) = self_exe.parent() {
            let candidate = dir.join(name);
            if candidate.exists() {
                return candidate;
            }
        }
    }
    PathBuf::from(name)
}

/// Spawn `synbadd` as a background process. Stdout+stderr are redirected to
/// `<state_dir>/synbadd.log` so the daemon's logs are recoverable but don't
/// mix with the GUI's own stderr. On Unix the child is placed in its own
/// process group so a terminal `Ctrl-C` against the GUI doesn't kill it.
///
/// We deliberately do not retain the `Child` handle: `Child` has no Drop
/// behaviour, so letting it fall out of scope simply detaches us from the
/// process — the daemon outlives the GUI on its own.
fn spawn_daemon() -> std::io::Result<std::process::Child> {
    let binary = synbadd_binary();
    let log_path = paths::state_dir().join("synbadd.log");
    if let Some(parent) = log_path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let (out, err) = (log_stdio(&log_path), log_stdio(&log_path));

    let mut cmd = Command::new(&binary);
    cmd.stdin(Stdio::null()).stdout(out).stderr(err);
    #[cfg(unix)]
    {
        cmd.process_group(0);
    }
    cmd.spawn()
}

fn log_stdio(path: &Path) -> Stdio {
    std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .map(Stdio::from)
        .unwrap_or_else(|_| Stdio::null())
}

fn command_loop(
    socket_path: PathBuf,
    cmd_rx: Receiver<Cmd>,
    update_tx: Sender<Update>,
    repaint: Arc<dyn Fn() + Send + Sync>,
) {
    while let Ok(cmd) = cmd_rx.recv() {
        match Connection::connect(&socket_path) {
            Ok(mut conn) => {
                let req = match cmd {
                    Cmd::Start => Request::Start,
                    Cmd::Stop => Request::Stop,
                    Cmd::Restart => Request::Restart,
                    Cmd::SetConfig(config) => Request::SetConfig { config },
                    Cmd::Refresh => Request::GetStatus,
                    Cmd::RefreshPeers => Request::ListPeers,
                    Cmd::StartPairing { machine_id } => Request::StartPairing { machine_id },
                    Cmd::ConfirmPairing { session_id, accept } => {
                        Request::ConfirmPairing { session_id, accept }
                    }
                    Cmd::RevokeTrust { machine_id } => Request::RevokeTrust { machine_id },
                };
                match conn.request(req) {
                    Ok(Response::Status { state, recent_log }) => {
                        let _ = update_tx.send(Update::Status { state, recent_log });
                    }
                    Ok(Response::Config { config }) => {
                        let _ = update_tx.send(Update::Config(config));
                    }
                    Ok(Response::Peers { peers }) => {
                        let _ = update_tx.send(Update::PeerSnapshot(peers));
                    }
                    Ok(Response::LocalIdentity { machine_id, fingerprint }) => {
                        let _ = update_tx
                            .send(Update::LocalIdentity { machine_id, fingerprint });
                    }
                    Ok(Response::TrustedPeers { peers }) => {
                        let _ = update_tx.send(Update::TrustedSnapshot(peers));
                    }
                    Ok(_) => {}
                    Err(e) => {
                        let _ = update_tx.send(Update::Error(e.to_string()));
                    }
                }
                repaint();
            }
            Err(e) => {
                let _ = update_tx.send(Update::Error(format!("dispatch connect: {}", e)));
                repaint();
            }
        }
    }
}

fn bootstrap(conn: &mut Connection, update_tx: &Sender<Update>) -> Result<(), String> {
    let cfg = conn.request(Request::GetConfig).map_err(|e| e.to_string())?;
    if let Response::Config { config } = cfg {
        let _ = update_tx.send(Update::Config(config));
    }
    let st = conn.request(Request::GetStatus).map_err(|e| e.to_string())?;
    if let Response::Status { state, recent_log } = st {
        let _ = update_tx.send(Update::Status { state, recent_log });
    }
    // Identity is small and never changes during the daemon's lifetime;
    // pull it once at connect time.
    if let Ok(Response::LocalIdentity { machine_id, fingerprint }) =
        conn.request(Request::GetLocalIdentity)
    {
        let _ = update_tx.send(Update::LocalIdentity { machine_id, fingerprint });
    }
    // Peers may already be visible if the daemon has been running a while.
    if let Ok(Response::Peers { peers }) = conn.request(Request::ListPeers) {
        let _ = update_tx.send(Update::PeerSnapshot(peers));
    }
    // Trusted-peer set: small, pulled once at connect.
    if let Ok(Response::TrustedPeers { peers }) = conn.request(Request::ListTrustedPeers) {
        let _ = update_tx.send(Update::TrustedSnapshot(peers));
    }
    Ok(())
}

fn refetch_config(socket_path: &PathBuf) -> Option<Config> {
    let mut c = Connection::connect(socket_path).ok()?;
    let r = c.request(Request::GetConfig).ok()?;
    if let Response::Config { config } = r {
        Some(config)
    } else {
        None
    }
}
