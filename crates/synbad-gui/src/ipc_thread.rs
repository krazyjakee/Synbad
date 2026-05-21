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
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

#[cfg(unix)]
use std::os::unix::process::CommandExt;

use crossbeam_channel::{Receiver, Sender};

use synbad_config::AudioConfig;
use synbad_config::{paths, Config};
use synbad_ipc::client::Connection;
use synbad_ipc::{
    AudioDeviceInfo, DaemonState, DiscoveredPeer, Event, Message, PeerAudioStatus, Request,
    Response, SyncDirection, TrustedPeer,
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
    StartPairing {
        machine_id: String,
    },
    /// Reply to a `PairingProposed` event with the user's verdict.
    ConfirmPairing {
        session_id: String,
        accept: bool,
    },
    /// Forget a previously-paired peer.
    RevokeTrust {
        machine_id: String,
    },
    /// Refresh the audio device dropdowns.
    ListAudioDevices,
    /// Push a new audio config — the daemon persists it and broadcasts a
    /// ConfigChanged event back so other tabs stay in sync.
    SetAudioConfig(AudioConfig),
    /// Pull current per-peer audio session state.
    GetAudioStatus,
}

#[derive(Debug, Clone)]
pub enum Update {
    Connected,
    Disconnected(String),
    /// The event loop has fired off a `synbadd` spawn and is waiting for
    /// its socket to come up. Rendered as a neutral status (not an error)
    /// so the user sees "starting" instead of a misleading "could not
    /// connect" during the boot window.
    Launching(String),
    Status {
        state: DaemonState,
        recent_log: Vec<String>,
    },
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
    LocalIdentity {
        machine_id: String,
        fingerprint: String,
    },
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
    PairingFailed {
        session_id: String,
        reason: String,
    },
    /// Replacement snapshot of the trusted-peer set.
    TrustedSnapshot(Vec<TrustedPeer>),
    /// A peer's trust was revoked.
    TrustRevoked(String),
    /// A config-sync session opened with a peer. The GUI shows a chip
    /// while at least one is active.
    SyncStarted {
        peer_machine_id: String,
        direction: SyncDirection,
    },
    /// A config-sync session completed. `updated` is true iff the merge
    /// actually changed something locally.
    SyncCompleted {
        peer_machine_id: String,
        direction: SyncDirection,
        updated: bool,
    },
    /// A config-sync session failed (connection, signature, etc.).
    SyncFailed {
        peer_machine_id: String,
        direction: SyncDirection,
        reason: String,
    },
    /// Reply to `Cmd::ListAudioDevices`. Includes both input and output
    /// devices the local cpal host can see.
    AudioDevices {
        input: Vec<AudioDeviceInfo>,
        output: Vec<AudioDeviceInfo>,
    },
    /// Daemon notified us the device set changed (plug/unplug).
    AudioDevicesChanged,
    /// Per-peer audio session status update.
    AudioPeerStatus(PeerAudioStatus),
    /// Peer's audio session ended — the GUI should drop its row from
    /// the per-peer status table.
    AudioPeerRemoved(String),
    /// Audio subsystem error. `peer` is `None` for global failures.
    AudioError {
        peer: Option<String>,
        message: String,
    },
    /// Reply to `Cmd::GetAudioStatus` — full snapshot.
    AudioStatusSnapshot(Vec<PeerAudioStatus>),
}

pub struct IpcHandle {
    pub cmd_tx: Sender<Cmd>,
    pub update_rx: Receiver<Update>,
    /// Session-scoped "user wants the daemon up right now" flag. Initialized
    /// from the on-disk `autostart` config at GUI startup; flipped to `true`
    /// when the user clicks Start so a disconnect mid-session triggers a
    /// re-spawn even if `autostart` is off. The event loop reads this on
    /// every reconnect attempt to decide whether to launch a fresh
    /// `synbadd`.
    pub daemon_wanted: Arc<AtomicBool>,
}

/// Best-effort synchronous daemon shutdown, used when the user quits the
/// GUI so a GUI-spawned `synbadd` doesn't outlive the window. Done on a
/// throwaway thread with a hard deadline: a missing daemon makes
/// `connect` fail fast, and a wedged one can't stall the quit past the
/// timeout. Errors are intentionally swallowed — we're exiting anyway.
pub fn shutdown_daemon(socket_path: PathBuf) {
    let (done_tx, done_rx) = crossbeam_channel::bounded::<()>(1);
    let spawned = thread::Builder::new()
        .name("synbad-shutdown".into())
        .spawn(move || {
            if let Ok(mut conn) = Connection::connect(&socket_path) {
                let _ = conn.request(Request::Shutdown);
            }
            let _ = done_tx.send(());
        });
    if spawned.is_ok() {
        let _ = done_rx.recv_timeout(Duration::from_millis(1500));
    }
}

pub fn spawn(socket_path: PathBuf, repaint: Arc<dyn Fn() + Send + Sync>) -> IpcHandle {
    let (cmd_tx, cmd_rx) = crossbeam_channel::unbounded::<Cmd>();
    let (update_tx, update_rx) = crossbeam_channel::unbounded::<Update>();
    let daemon_wanted = Arc::new(AtomicBool::new(autostart_at_startup()));

    {
        let socket_path = socket_path.clone();
        let update_tx = update_tx.clone();
        let repaint = repaint.clone();
        let daemon_wanted = daemon_wanted.clone();
        thread::Builder::new()
            .name("synbad-ipc-events".into())
            .spawn(move || event_loop(socket_path, update_tx, repaint, daemon_wanted))
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

    IpcHandle {
        cmd_tx,
        update_rx,
        daemon_wanted,
    }
}

/// How long after we initiate a `synbadd` spawn we keep showing a soft
/// "starting" status instead of a "could not connect" error. The daemon
/// typically binds its socket in well under a second; 5s gives plenty of
/// headroom for cold-cache disk reads and Windows AV scans without
/// letting a truly-broken binary hide behind the friendly message.
const LAUNCH_GRACE: Duration = Duration::from_secs(5);
/// Minimum gap between consecutive spawn attempts. Stops a daemon that
/// crashes on startup from getting respawned every backoff tick (which
/// would also accumulate zombie processes on Unix).
const SPAWN_THROTTLE: Duration = Duration::from_secs(3);

/// True iff a spawn attempt at `last_spawn_at` is still within the soft
/// "starting" grace window relative to `now`. Used to decide whether a
/// connect failure should surface as a friendly `Launching` status or a
/// real `Disconnected` error.
fn in_launch_grace(last_spawn_at: Option<Instant>, now: Instant, grace: Duration) -> bool {
    last_spawn_at
        .map(|t| now.saturating_duration_since(t) < grace)
        .unwrap_or(false)
}

/// True iff we should kick off a fresh `synbadd` spawn this iteration:
/// the user wants a daemon running AND we haven't tried recently (so a
/// crashing daemon doesn't get re-spawned on every backoff tick).
fn should_spawn(daemon_wanted: bool, last_spawn_at: Option<Instant>, now: Instant) -> bool {
    daemon_wanted
        && last_spawn_at
            .map(|t| now.saturating_duration_since(t) >= SPAWN_THROTTLE)
            .unwrap_or(true)
}

/// Render the "couldn't even launch the binary" failure into a
/// user-facing string. Kept as a free function so the message format is
/// trivially testable without touching the surrounding IPC machinery.
fn launch_failure_msg(path: &Path, err: &std::io::Error) -> String {
    format!("could not launch synbadd at {}: {}", path.display(), err)
}

fn event_loop(
    socket_path: PathBuf,
    update_tx: Sender<Update>,
    repaint: Arc<dyn Fn() + Send + Sync>,
    daemon_wanted: Arc<AtomicBool>,
) {
    let mut backoff = Duration::from_millis(500);
    let mut last_spawn: Option<Instant> = None;
    // Sticky message from the most recent failed spawn. Outranks the
    // generic "could not connect" — a missing binary is the user-actionable
    // root cause and the connect failure is just downstream noise. Cleared
    // when a spawn succeeds or a connect succeeds.
    let mut last_spawn_error: Option<String> = None;

    // Kick off the daemon BEFORE the first connect attempt when the user
    // wants one running. Without this we'd flash "could not connect to
    // synbadd" on every cold start with autostart on — the connect
    // races ahead of the spawn we were about to do anyway.
    if daemon_wanted.load(Ordering::Relaxed) {
        last_spawn_error = try_spawn(&update_tx, &repaint).err();
        last_spawn = Some(Instant::now());
        if last_spawn_error.is_none() {
            let _ = update_tx.send(Update::Launching("starting synbadd…".into()));
            repaint();
        }
    }

    loop {
        match Connection::connect(&socket_path) {
            Ok(mut conn) => {
                last_spawn_error = None;
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
                                Event::PeerDisconnected { name } => Update::PeerDisconnected(name),
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
                                Event::SyncStarted {
                                    peer_machine_id,
                                    direction,
                                } => Update::SyncStarted {
                                    peer_machine_id,
                                    direction,
                                },
                                Event::SyncCompleted {
                                    peer_machine_id,
                                    direction,
                                    updated,
                                    new_head: _,
                                } => Update::SyncCompleted {
                                    peer_machine_id,
                                    direction,
                                    updated,
                                },
                                Event::SyncFailed {
                                    peer_machine_id,
                                    direction,
                                    reason,
                                } => Update::SyncFailed {
                                    peer_machine_id,
                                    direction,
                                    reason,
                                },
                                Event::AudioDevicesChanged => Update::AudioDevicesChanged,
                                Event::AudioPeerStatus { status } => {
                                    Update::AudioPeerStatus(status)
                                }
                                Event::AudioPeerRemoved { machine_id } => {
                                    Update::AudioPeerRemoved(machine_id)
                                }
                                Event::AudioError { peer, message } => {
                                    Update::AudioError { peer, message }
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
                // Pick the most informative status for the user:
                //   1. If our most recent spawn failed outright (e.g. the
                //      binary doesn't exist), that's the root cause —
                //      surface it instead of the generic connect error.
                //   2. If we recently fired off a spawn that succeeded, the
                //      socket just isn't bound yet — show "starting…", not
                //      a scary error.
                //   3. Otherwise this is a genuine connect failure: no
                //      daemon, no recent spawn we initiated, daemon_wanted
                //      may be off. Show the real error.
                let now = Instant::now();
                if let Some(msg) = last_spawn_error.clone() {
                    let _ = update_tx.send(Update::Disconnected(msg));
                } else if in_launch_grace(last_spawn, now, LAUNCH_GRACE) {
                    let _ = update_tx.send(Update::Launching("starting synbadd…".into()));
                } else {
                    let _ = update_tx.send(Update::Disconnected(format!(
                        "could not connect to synbadd at {:?}: {}",
                        socket_path, e
                    )));
                }
                repaint();

                // Auto-launch the daemon if nothing is listening, but only
                // when the user wants one running. `daemon_wanted` starts
                // life from the on-disk `autostart` flag and is flipped
                // on by the Start button — so an autostart=off user who
                // has *also* never clicked Start gets a quiet GUI, while
                // a daemon that crashes mid-session is respawned for both
                // autostart modes (the user clearly wanted it up).
                if should_spawn(daemon_wanted.load(Ordering::Relaxed), last_spawn, now) {
                    last_spawn_error = try_spawn(&update_tx, &repaint).err();
                    last_spawn = Some(Instant::now());
                }
            }
        }

        thread::sleep(backoff);
        backoff = (backoff * 2).min(Duration::from_secs(10));
    }
}

/// Read `autostart` straight off the on-disk config to seed the
/// session-scoped `daemon_wanted` flag at GUI startup. We can't ask the
/// daemon yet (there isn't one), so the on-disk value is the only source
/// of truth this early. Defaults to `true` when the file is missing or
/// malformed — first-run users (and anyone whose config got corrupted)
/// should still get the turnkey "GUI launches the daemon" experience.
fn autostart_at_startup() -> bool {
    match synbad_config::Config::load(&paths::config_file()) {
        Ok(Some(cfg)) => cfg.autostart,
        Ok(None) => true,
        Err(e) => {
            tracing::warn!(?e, "could not read autostart from config; defaulting to on");
            true
        }
    }
}

/// Locate the `synbadd` executable: prefer one alongside our own binary
/// (typical for both `cargo build` layouts and installed bundles), and fall
/// back to PATH lookup so a system-installed daemon still works.
fn synbadd_binary() -> PathBuf {
    let name = if cfg!(windows) {
        "synbadd.exe"
    } else {
        "synbadd"
    };
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
///
/// Returns the path we attempted to launch alongside the spawn result so
/// callers can show `could not launch synbadd at <path>` without re-resolving.
fn spawn_daemon() -> (PathBuf, std::io::Result<std::process::Child>) {
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
    (binary, cmd.spawn())
}

/// One-shot wrapper around `spawn_daemon` that also surfaces failures to
/// the UI. On success returns `Ok(())` — the caller is responsible for
/// emitting `Update::Launching` if it wants the boot-status banner. On
/// failure, returns the user-facing message we already pushed onto the
/// update channel so the caller can keep it as `last_spawn_error`.
fn try_spawn(
    update_tx: &Sender<Update>,
    repaint: &Arc<dyn Fn() + Send + Sync>,
) -> Result<(), String> {
    let (binary, result) = spawn_daemon();
    match result {
        Ok(child) => {
            tracing::info!(pid = child.id(), path = %binary.display(), "spawned synbadd");
            Ok(())
        }
        Err(e) => {
            let msg = launch_failure_msg(&binary, &e);
            tracing::warn!(?e, path = %binary.display(), "failed to spawn synbadd");
            let _ = update_tx.send(Update::Disconnected(msg.clone()));
            repaint();
            Err(msg)
        }
    }
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
    // How long a single command will wait for the daemon to come up
    // before we give up. Covers the typical Start-button-while-daemon-is-
    // booting race: the event loop spawns `synbadd`, the daemon boots and
    // binds its socket in ~hundreds of ms, and we want the Cmd::Start the
    // user just clicked to land instead of getting dropped on the floor.
    const COMMAND_CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
    const COMMAND_CONNECT_RETRY: Duration = Duration::from_millis(150);

    while let Ok(cmd) = cmd_rx.recv() {
        let started = Instant::now();
        let conn_result = loop {
            match Connection::connect(&socket_path) {
                Ok(c) => break Ok(c),
                Err(e) if started.elapsed() >= COMMAND_CONNECT_TIMEOUT => break Err(e),
                Err(_) => thread::sleep(COMMAND_CONNECT_RETRY),
            }
        };
        match conn_result {
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
                    Cmd::ListAudioDevices => Request::ListAudioDevices,
                    Cmd::SetAudioConfig(config) => Request::SetAudioConfig { config },
                    Cmd::GetAudioStatus => Request::GetAudioStatus,
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
                    Ok(Response::LocalIdentity {
                        machine_id,
                        fingerprint,
                    }) => {
                        let _ = update_tx.send(Update::LocalIdentity {
                            machine_id,
                            fingerprint,
                        });
                    }
                    Ok(Response::TrustedPeers { peers }) => {
                        let _ = update_tx.send(Update::TrustedSnapshot(peers));
                    }
                    Ok(Response::AudioDevices { input, output }) => {
                        let _ = update_tx.send(Update::AudioDevices { input, output });
                    }
                    Ok(Response::AudioStatus { peers }) => {
                        let _ = update_tx.send(Update::AudioStatusSnapshot(peers));
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
    let cfg = conn
        .request(Request::GetConfig)
        .map_err(|e| e.to_string())?;
    if let Response::Config { config } = cfg {
        let _ = update_tx.send(Update::Config(config));
    }
    let st = conn
        .request(Request::GetStatus)
        .map_err(|e| e.to_string())?;
    if let Response::Status { state, recent_log } = st {
        let _ = update_tx.send(Update::Status { state, recent_log });
    }
    // Identity is small and never changes during the daemon's lifetime;
    // pull it once at connect time.
    if let Ok(Response::LocalIdentity {
        machine_id,
        fingerprint,
    }) = conn.request(Request::GetLocalIdentity)
    {
        let _ = update_tx.send(Update::LocalIdentity {
            machine_id,
            fingerprint,
        });
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

fn refetch_config(socket_path: &Path) -> Option<Config> {
    let mut c = Connection::connect(socket_path).ok()?;
    let r = c.request(Request::GetConfig).ok()?;
    if let Response::Config { config } = r {
        Some(config)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn synbadd_binary_uses_platform_name() {
        // synbadd_binary resolves to either an adjacent candidate or a
        // bare PathBuf — but the file name should always match the
        // platform-appropriate executable name.
        let expected = if cfg!(windows) {
            "synbadd.exe"
        } else {
            "synbadd"
        };
        let path = synbadd_binary();
        assert_eq!(
            path.file_name().and_then(|s| s.to_str()),
            Some(expected),
            "synbadd_binary should resolve to a path whose file name is {expected:?}, got {path:?}",
        );
    }

    #[test]
    fn in_launch_grace_no_attempt_is_false() {
        // Before any spawn has been kicked off there is no grace window
        // — a connect failure here means "no daemon, nothing in flight"
        // and the user should see a real error.
        let now = Instant::now();
        assert!(!in_launch_grace(None, now, Duration::from_secs(5)));
    }

    #[test]
    fn in_launch_grace_recent_attempt_is_true() {
        // A spawn we kicked off well within the grace window suppresses
        // the connect-failure error in favour of the soft "starting…"
        // status.
        let now = Instant::now();
        let recent = now - Duration::from_millis(500);
        assert!(in_launch_grace(Some(recent), now, Duration::from_secs(5)));
    }

    #[test]
    fn in_launch_grace_stale_attempt_is_false() {
        // Once the grace window expires we should fall through to the
        // real "could not connect" error so a daemon that never bound
        // its socket doesn't hide behind the friendly status forever.
        let now = Instant::now();
        let stale = now - Duration::from_secs(30);
        assert!(!in_launch_grace(Some(stale), now, Duration::from_secs(5)));
    }

    #[test]
    fn should_spawn_skips_when_daemon_not_wanted() {
        // The user has autostart off and hasn't clicked Start — a connect
        // failure is normal and we shouldn't be poking at the binary.
        let now = Instant::now();
        assert!(!should_spawn(false, None, now));
        assert!(!should_spawn(
            false,
            Some(now - Duration::from_secs(60)),
            now
        ));
    }

    #[test]
    fn should_spawn_fires_on_first_attempt() {
        // First iteration with daemon_wanted=true and no recent spawn —
        // this is the path that fixes the original "could not connect
        // before launch" race.
        let now = Instant::now();
        assert!(should_spawn(true, None, now));
    }

    #[test]
    fn should_spawn_is_throttled() {
        // A daemon that crashes on startup shouldn't get respawned
        // every backoff tick — the throttle window protects us from
        // zombie accumulation on Unix.
        let now = Instant::now();
        let very_recent = now - Duration::from_millis(100);
        assert!(!should_spawn(true, Some(very_recent), now));

        let just_after_throttle = now - (SPAWN_THROTTLE + Duration::from_millis(100));
        assert!(should_spawn(true, Some(just_after_throttle), now));
    }

    #[test]
    fn launch_failure_msg_includes_path_and_io_error() {
        // The user needs to know which binary we tried to launch (so they
        // can verify it exists at that path), and which OS error came
        // back. No dev-time build hints — keep the message factual.
        let err = std::io::Error::new(std::io::ErrorKind::NotFound, "No such file or directory");
        let msg = launch_failure_msg(Path::new("/usr/bin/synbadd"), &err);
        assert!(
            msg.contains("/usr/bin/synbadd"),
            "expected attempted path in message, got {msg:?}",
        );
        assert!(
            msg.contains("No such file or directory"),
            "expected underlying io error in message, got {msg:?}",
        );
        assert!(
            msg.starts_with("could not launch synbadd"),
            "expected user-facing prefix in message, got {msg:?}",
        );
    }
}
