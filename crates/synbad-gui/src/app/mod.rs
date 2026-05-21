//! Top-level egui app: tab bar, state, IPC plumbing.
//!
//! [`SynbadApp`] is the long-lived value `eframe` drives. It owns the
//! cached daemon snapshot, edit buffers, and the worker channels feeding
//! it (IPC, tray, single-instance SHOW pings, update flow).
//!
//! This file holds the struct, its constructor, the per-frame `update`
//! method, and the small helpers that mutate state in response to
//! channel/tray events. The actual tab rendering (`draw_status`,
//! `draw_layout`, …) and the small UI free helpers live in
//! [`views`] so the impl that drives the frame loop stays scannable.

mod views;

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::sync::Arc;
use std::time::Instant;

use eframe::CreationContext;

use synbad_config::{paths, Config, MonitorInfo, NodeRole, Screen};
use synbad_ipc::{
    AudioDeviceInfo, DaemonState, DiscoveredPeer, PeerAudioStatus, SyncDirection, TrustedPeer,
};

use crate::ipc_thread::{self, Cmd, IpcHandle, Update};
use crate::layout_editor::LayoutEditor;
use crate::monitors;
use crate::tray;
use crate::update::{self, UpdateState};

/// How often the GUI re-checks for monitor changes and unmapped trusted
/// peers. Cheap to run — display-info is a couple of syscalls and the
/// peer pass is O(trusted_peers). Picked to be responsive to a user
/// plugging/unplugging a monitor without flooding the daemon with
/// SetConfig calls.
const RECONCILE_INTERVAL_SECS: u64 = 5;

pub(super) const LOG_CAP: usize = 1000;

#[derive(PartialEq, Eq)]
pub(super) enum Tab {
    Status,
    Layout,
    Peers,
    Audio,
    Settings,
}

/// One pending pairing session waiting for the user's accept/decline.
#[derive(Debug, Clone)]
pub struct PendingPairing {
    pub session_id: String,
    pub peer_machine_id: String,
    pub peer_display_name: String,
    pub peer_fingerprint: String,
    pub verification_code: String,
}

pub struct SynbadApp {
    pub(super) ipc: IpcHandle,
    pub(super) tab: Tab,

    pub(super) connected: bool,
    pub(super) last_error: Option<String>,
    pub(super) state: DaemonState,
    pub(super) log: VecDeque<String>,

    // Edited copy of the config. `dirty` tracks whether it diverges from
    // what the daemon last reported.
    pub(super) config: Config,
    pub(super) dirty: bool,
    // A config update that arrived from the daemon while the user had
    // unsaved local edits. Surfaced as a banner so the user knows their
    // view is stale; Revert pulls it in without round-tripping the daemon.
    pub(super) pending_remote_config: Option<Config>,

    pub(super) layout: LayoutEditor,
    pub(super) new_screen_name: String,

    /// Names of remote screens currently handshake-confirmed by the Core.
    /// Derived from log-line parsing in the daemon — not authoritative.
    pub(super) connected_peers: BTreeSet<String>,
    /// Name of the screen the cursor is currently on (server role).
    pub(super) active_screen: Option<String>,

    /// LAN-discovered peers from mDNS, keyed by machine_id (sorted via
    /// BTreeMap so the GUI renders in a stable order).
    pub(super) discovered_peers: BTreeMap<String, DiscoveredPeer>,
    /// Peers the user has paired with, keyed by machine_id.
    pub(super) trusted_peers: BTreeMap<String, TrustedPeer>,
    /// Pairing sessions awaiting the user's accept/decline. We render one
    /// floating dialog per entry so multiple inbound requests are all
    /// visible.
    pub(super) pending_pairings: Vec<PendingPairing>,
    /// Local machine UUID and short fingerprint for the "this is us"
    /// header in the Peers tab.
    pub(super) local_machine_id: Option<String>,
    pub(super) local_fingerprint: Option<String>,

    /// Tray installed → close button minimizes to tray instead of exiting.
    pub(super) has_tray: bool,
    /// User explicitly chose Quit (via tray or future menu item). Lets us
    /// distinguish a real exit from a close-to-tray click.
    pub(super) quitting: bool,
    /// Set once we've told the daemon to shut down, so a multi-frame close
    /// (tray Quit queues a Close that lands next frame) doesn't fire the
    /// blocking shutdown request more than once.
    pub(super) daemon_shutdown_sent: bool,
    /// Receiver fed by the single-instance listener thread. A second
    /// launcher click ping arrives here; we react by raising and focusing
    /// the window, same as the tray's "Show Synbad".
    pub(super) show_rx: Option<crossbeam_channel::Receiver<()>>,

    /// Peers we currently have an open sync session with, keyed by
    /// machine_id with their direction so the UI can show "syncing"
    /// while the session is in flight.
    pub(super) active_syncs: BTreeMap<String, SyncDirection>,
    /// Most recent sync outcome. Shown as a short-lived chip beside the
    /// status indicator so the user knows when a layout change has
    /// reached (or failed to reach) the other side.
    pub(super) last_sync_status: Option<SyncStatus>,

    /// Whether the Updates modal is currently visible. Toggled by the tray
    /// menu's "Check for updates…" entry and the Settings tab button.
    pub(super) show_update_dialog: bool,
    /// State machine for the in-progress update flow (check or install).
    /// `None` means we haven't opened the dialog yet this session.
    pub(super) update_state: Option<UpdateState>,

    /// Snapshot of locally-attached monitors, refreshed periodically.
    /// Mirrored into `config.screens[server_name].monitors` whenever a
    /// difference is detected so the layout reflects real hardware.
    pub(super) local_monitors: Vec<MonitorInfo>,
    /// Wall-clock of the most recent monitor / auto-populate pass. Drives
    /// the [`RECONCILE_INTERVAL_SECS`] tick from inside `update`.
    pub(super) last_reconcile: Option<Instant>,

    /// Cached audio device list, refreshed on entering the Audio tab and
    /// when the daemon reports a `DevicesChanged` event.
    pub(super) audio_input_devices: Vec<AudioDeviceInfo>,
    pub(super) audio_output_devices: Vec<AudioDeviceInfo>,
    /// Per-peer audio session status, keyed by machine_id.
    pub(super) audio_peer_status: BTreeMap<String, PeerAudioStatus>,
    /// Sticky audio error from the daemon, dismissable in the Audio tab.
    pub(super) audio_last_error: Option<String>,
    /// True once we've fetched the device list at least once. Lets the
    /// tab show "loading…" until the daemon replies.
    pub(super) audio_devices_loaded: bool,

    /// Reveal the technical knobs (role, ports, server address, core path,
    /// generated `.conf` previews, raw log) in the Settings + Status tabs.
    /// Off by default — the visible UI is dashboard-style, advanced is a
    /// reveal for power users. Session-scoped (no persistence) so we don't
    /// silently dump advanced UI on someone who toggled it once.
    pub(super) show_advanced: bool,
}

#[derive(Debug, Clone)]
pub(super) struct SyncStatus {
    /// Human-readable message ("synced with macbook" / "sync to laptop failed: …").
    pub(super) message: String,
    /// Whether the underlying event was a success or failure — drives the chip colour.
    pub(super) ok: bool,
    /// Wall-clock time we observed the event. The chip auto-clears after a few seconds.
    pub(super) at: Instant,
}

impl SynbadApp {
    pub fn new(
        cc: &CreationContext<'_>,
        has_tray: bool,
        show_rx: Option<crossbeam_channel::Receiver<()>>,
    ) -> Self {
        let egui_ctx = cc.egui_ctx.clone();
        let repaint: Arc<dyn Fn() + Send + Sync> = Arc::new(move || egui_ctx.request_repaint());
        let ipc = ipc_thread::spawn(paths::ipc_socket(), repaint);

        SynbadApp {
            ipc,
            tab: Tab::Status,
            connected: false,
            last_error: None,
            state: DaemonState::Stopped,
            log: VecDeque::with_capacity(LOG_CAP),
            config: Config::default(),
            dirty: false,
            pending_remote_config: None,
            layout: LayoutEditor::default(),
            new_screen_name: String::new(),
            connected_peers: BTreeSet::new(),
            active_screen: None,
            discovered_peers: BTreeMap::new(),
            trusted_peers: BTreeMap::new(),
            pending_pairings: Vec::new(),
            local_machine_id: None,
            local_fingerprint: None,
            has_tray,
            quitting: false,
            daemon_shutdown_sent: false,
            show_rx,
            active_syncs: BTreeMap::new(),
            last_sync_status: None,
            show_update_dialog: false,
            update_state: None,
            local_monitors: monitors::enumerate(),
            last_reconcile: None,
            audio_input_devices: Vec::new(),
            audio_output_devices: Vec::new(),
            audio_peer_status: BTreeMap::new(),
            audio_last_error: None,
            audio_devices_loaded: false,
            show_advanced: false,
        }
    }

    /// Drain SHOW pings from the single-instance listener and raise the
    /// window for each one. Always called in the update loop so a second
    /// launcher click immediately surfaces this process.
    fn drain_show_requests(&self, ctx: &egui::Context) {
        let Some(rx) = self.show_rx.as_ref() else {
            return;
        };
        let mut raised = false;
        while let Ok(()) = rx.try_recv() {
            raised = true;
        }
        if raised {
            ctx.send_viewport_cmd(egui::ViewportCommand::Visible(true));
            ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
        }
    }

    fn drain_tray(&mut self, ctx: &egui::Context) {
        if !self.has_tray {
            return;
        }
        while let Some(id) = tray::try_recv_menu_id() {
            match id.as_str() {
                tray::MENU_ID_SHOW => {
                    ctx.send_viewport_cmd(egui::ViewportCommand::Visible(true));
                    ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
                }
                tray::MENU_ID_CHECK_UPDATES => {
                    // Bring the window forward so the user sees the modal
                    // even if they clicked from a hidden/minimized state.
                    ctx.send_viewport_cmd(egui::ViewportCommand::Visible(true));
                    ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
                    self.open_update_dialog(ctx);
                }
                tray::MENU_ID_QUIT => {
                    self.quitting = true;
                    ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                }
                _ => {}
            }
        }
    }

    /// Open the Updates modal and auto-kick a check if we don't already
    /// have a result on screen. Re-clicking while the modal is open is a
    /// no-op other than ensuring it's visible.
    pub(super) fn open_update_dialog(&mut self, ctx: &egui::Context) {
        self.show_update_dialog = true;
        let needs_check = matches!(
            self.update_state,
            None | Some(UpdateState::CheckFailed(_)) | Some(UpdateState::InstallFailed(_))
        );
        if needs_check {
            self.update_state = Some(update::spawn_check(ctx));
        }
    }

    /// Intercept the window close to minimize-to-tray, unless the user
    /// explicitly chose Quit. Tray-less builds get default close behavior.
    /// On a real exit (no tray, or Quit chosen) we take the daemon down
    /// with us so a GUI-spawned `synbadd` doesn't linger.
    fn handle_close(&mut self, ctx: &egui::Context) {
        let close_requested = ctx.input(|i| i.viewport().close_requested());
        if self.has_tray && !self.quitting {
            if close_requested {
                ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
                ctx.send_viewport_cmd(egui::ViewportCommand::Visible(false));
            }
            return;
        }
        if close_requested || self.quitting {
            self.shutdown_daemon_once();
        }
    }

    /// Tell the daemon to exit, at most once per process. Blocking but
    /// bounded (see [`ipc_thread::shutdown_daemon`]); only ever called on
    /// the way out. Always runs — `synbadd` is GUI-only, so when the GUI
    /// closes the daemon should go with it regardless of the autostart
    /// setting (which controls *startup*, not shutdown).
    fn shutdown_daemon_once(&mut self) {
        if self.daemon_shutdown_sent {
            return;
        }
        self.daemon_shutdown_sent = true;
        ipc_thread::shutdown_daemon(paths::ipc_socket());
    }

    fn drain_ipc(&mut self) {
        while let Ok(upd) = self.ipc.update_rx.try_recv() {
            match upd {
                Update::Connected => {
                    self.connected = true;
                    self.last_error = None;
                }
                Update::Disconnected(msg) => {
                    self.connected = false;
                    self.last_error = Some(msg);
                }
                Update::Status { state, recent_log } => {
                    self.state = state;
                    self.log.clear();
                    for l in recent_log {
                        self.push_log(l);
                    }
                }
                Update::Config(c) => {
                    if self.dirty {
                        // Don't clobber unsaved local edits — but remember
                        // the remote update so we can surface a banner and
                        // load it on Revert.
                        self.pending_remote_config = Some(c);
                    } else {
                        self.config = c;
                        self.pending_remote_config = None;
                    }
                }
                Update::Log(l) => self.push_log(l),
                Update::StateChanged(s) => {
                    // The Core is gone — peers can't survive that.
                    if !s.is_running() {
                        self.connected_peers.clear();
                        self.active_screen = None;
                    }
                    self.state = s;
                }
                Update::Error(e) => self.last_error = Some(e),
                Update::PeerConnected(name) => {
                    self.connected_peers.insert(name);
                }
                Update::PeerDisconnected(name) => {
                    self.connected_peers.remove(&name);
                }
                Update::ActiveScreen(name) => {
                    self.active_screen = Some(name);
                }
                Update::PeerDiscovered(peer) => {
                    self.discovered_peers.insert(peer.machine_id.clone(), peer);
                }
                Update::PeerLost(id) => {
                    self.discovered_peers.remove(&id);
                }
                Update::PeerSnapshot(peers) => {
                    self.discovered_peers.clear();
                    for p in peers {
                        self.discovered_peers.insert(p.machine_id.clone(), p);
                    }
                }
                Update::LocalIdentity {
                    machine_id,
                    fingerprint,
                } => {
                    self.local_machine_id = Some(machine_id);
                    self.local_fingerprint = Some(fingerprint);
                }
                Update::PairingProposed {
                    session_id,
                    peer_machine_id,
                    peer_display_name,
                    peer_fingerprint,
                    verification_code,
                } => {
                    self.pending_pairings.push(PendingPairing {
                        session_id,
                        peer_machine_id,
                        peer_display_name,
                        peer_fingerprint,
                        verification_code,
                    });
                }
                Update::PairingCompleted(peer) => {
                    // Drop any pending dialog for this peer (the
                    // confirmation it represented just succeeded).
                    self.pending_pairings
                        .retain(|p| p.peer_machine_id != peer.machine_id);
                    self.trusted_peers.insert(peer.machine_id.clone(), peer);
                    // Force the next `update` frame to reconcile instead
                    // of waiting out the interval — the user expects the
                    // peer to slot into their layout immediately.
                    self.last_reconcile = None;
                }
                Update::PairingFailed { session_id, reason } => {
                    self.pending_pairings.retain(|p| p.session_id != session_id);
                    self.last_error = Some(format!("pairing failed: {}", reason));
                }
                Update::TrustedSnapshot(peers) => {
                    self.trusted_peers.clear();
                    for p in peers {
                        self.trusted_peers.insert(p.machine_id.clone(), p);
                    }
                }
                Update::TrustRevoked(id) => {
                    self.trusted_peers.remove(&id);
                }
                Update::SyncStarted {
                    peer_machine_id,
                    direction,
                } => {
                    self.active_syncs.insert(peer_machine_id, direction);
                }
                Update::SyncCompleted {
                    peer_machine_id,
                    direction,
                    updated,
                } => {
                    self.active_syncs.remove(&peer_machine_id);
                    let name = self.peer_label(&peer_machine_id);
                    let verb = match direction {
                        SyncDirection::Outbound => "pushed to",
                        SyncDirection::Inbound => "received from",
                    };
                    let detail = if updated { "merged" } else { "no changes" };
                    self.last_sync_status = Some(SyncStatus {
                        message: format!("config {} {} ({})", verb, name, detail),
                        ok: true,
                        at: Instant::now(),
                    });
                }
                Update::SyncFailed {
                    peer_machine_id,
                    direction,
                    reason,
                } => {
                    self.active_syncs.remove(&peer_machine_id);
                    let name = self.peer_label(&peer_machine_id);
                    let dir = match direction {
                        SyncDirection::Outbound => "to",
                        SyncDirection::Inbound => "from",
                    };
                    self.last_sync_status = Some(SyncStatus {
                        message: format!("config sync {} {} failed: {}", dir, name, reason),
                        ok: false,
                        at: Instant::now(),
                    });
                }
                Update::AudioDevices { input, output } => {
                    self.audio_input_devices = input;
                    self.audio_output_devices = output;
                    self.audio_devices_loaded = true;
                }
                Update::AudioDevicesChanged => {
                    // Eagerly refresh — the daemon's heads-up doesn't carry
                    // the new list, and a stale dropdown is worse than a
                    // small extra IPC call.
                    self.send(Cmd::ListAudioDevices);
                }
                Update::AudioPeerStatus(status) => {
                    self.audio_peer_status
                        .insert(status.machine_id.clone(), status);
                }
                Update::AudioStatusSnapshot(peers) => {
                    self.audio_peer_status.clear();
                    for p in peers {
                        self.audio_peer_status.insert(p.machine_id.clone(), p);
                    }
                }
                Update::AudioError { peer, message } => {
                    self.audio_last_error = Some(match peer {
                        Some(p) => format!("{}: {}", self.peer_label(&p), message),
                        None => message,
                    });
                }
            }
        }
    }

    /// Switch this machine into client mode pointed at `peer`. Replaces
    /// the three-step Settings dance (change role → paste IP → Apply)
    /// with a single click from the Peers tab. Also kicks Start so the
    /// Core actually dials the new address — without this the user would
    /// still have to bounce down to the bottom bar.
    pub(super) fn connect_as_client_to(&mut self, peer: &DiscoveredPeer) {
        let addr = format!("{}:{}", peer.host, peer.core_port);
        let mut new_cfg = self.config.clone();
        new_cfg.role = NodeRole::Client;
        new_cfg.server_address = Some(addr);
        self.send(Cmd::SetConfig(new_cfg.clone()));
        self.config = new_cfg;
        self.dirty = false;
        self.pending_remote_config = None;
        self.send(Cmd::Start);
    }

    /// Display label for a peer, preferring the trusted/discovered display
    /// name and falling back to the raw machine_id if we have no metadata.
    pub(super) fn peer_label(&self, machine_id: &str) -> String {
        if let Some(p) = self.trusted_peers.get(machine_id) {
            return p.display_name.clone();
        }
        if let Some(p) = self.discovered_peers.get(machine_id) {
            return p.display_name.clone();
        }
        machine_id.to_string()
    }

    /// Periodic reconciliation pass. Two jobs:
    ///   * Refresh `local_monitors` and mirror them into our own `Screen`
    ///     entry so plugging/unplugging a display is reflected in the layout.
    ///   * Append any trusted peer that isn't already represented as a
    ///     screen, so paired-but-unseen machines auto-appear after the
    ///     interval even when `PairingCompleted` wasn't observed (e.g. the
    ///     user paired through another session and we connected later).
    ///
    /// Both rules only mutate the local edit buffer — they never call
    /// `SetConfig` unless the user already had unsaved edits *or* the
    /// change is a pure structural fix-up that should propagate. To avoid
    /// fighting the user mid-drag, we skip the layout-touching half while
    /// `self.layout` reports a drag in progress and while a "remote update
    /// arrived" banner is pending.
    fn reconcile(&mut self) {
        if !self.connected {
            return;
        }
        if self.pending_remote_config.is_some() {
            // Don't fight a banner we haven't resolved yet.
            return;
        }
        if self.layout.is_dragging() {
            return;
        }

        let mut changed = false;

        // Refresh local monitors. Re-enumeration is cheap; the diff lets
        // us avoid pointless SetConfig round-trips.
        let fresh = monitors::enumerate();
        if fresh != self.local_monitors {
            self.local_monitors = fresh;
        }

        if let Some(screen) = self
            .config
            .screens
            .iter_mut()
            .find(|s| s.name == self.config.server_name)
        {
            if screen.monitors != self.local_monitors {
                screen.monitors = self.local_monitors.clone();
                changed = true;
            }
        }

        // Auto-populate paired peers as screens. Server-driven: we only
        // append peers we've already paired with. The peer's own GUI is
        // responsible for filling in *its* monitors on its own pass — sync
        // delivers that update to us, and the per-field LWW resolves any
        // race.
        let known: BTreeSet<&str> = self
            .config
            .screens
            .iter()
            .map(|s| s.name.as_str())
            .collect();
        let mut to_add: Vec<String> = Vec::new();
        for peer in self.trusted_peers.values() {
            if peer.display_name == self.config.server_name {
                // The peer happens to share our name — skip rather than
                // collide. The user can rename either side.
                continue;
            }
            if !known.contains(peer.display_name.as_str()) {
                to_add.push(peer.display_name.clone());
            }
        }
        if !to_add.is_empty() {
            for name in to_add {
                let idx = self.config.screens.len() as i32;
                self.config.screens.push(Screen {
                    name,
                    aliases: vec![],
                    position: synbad_config::GridPosition {
                        x: (idx % 3) * 200,
                        y: (idx / 3) * 160,
                        w: 160,
                        h: 120,
                    },
                    monitors: vec![],
                });
            }
            changed = true;
        }

        if changed {
            // Push immediately rather than waiting for the user to hit
            // Apply: monitor info and auto-added peers are infra-level,
            // not creative edits, and persisting them is what makes the
            // sync visible on the other side.
            self.send(Cmd::SetConfig(self.config.clone()));
            self.dirty = false;
        }
    }

    fn push_log(&mut self, line: String) {
        if self.log.len() >= LOG_CAP {
            self.log.pop_front();
        }
        self.log.push_back(line);
    }

    pub(super) fn send(&self, cmd: Cmd) {
        let _ = self.ipc.cmd_tx.send(cmd);
    }
}

impl eframe::App for SynbadApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.drain_ipc();
        self.drain_tray(ctx);
        self.drain_show_requests(ctx);
        self.handle_close(ctx);
        update::poll(&mut self.update_state);

        // Periodic reconcile pass — monitor enumeration + paired-peer
        // backfill. Cheap, so the cadence is short.
        let should_reconcile = match self.last_reconcile {
            None => true,
            Some(t) => t.elapsed().as_secs() >= RECONCILE_INTERVAL_SECS,
        };
        if should_reconcile {
            self.last_reconcile = Some(Instant::now());
            self.reconcile();
        }
        // Keep ticking even when the user isn't interacting so monitor
        // changes (plug/unplug) and newly-paired peers show up without
        // requiring a click.
        ctx.request_repaint_after(std::time::Duration::from_secs(RECONCILE_INTERVAL_SECS));
        // While a worker is running, ask egui to repaint at ~30 Hz so the
        // progress bar moves on its own without depending on user input.
        if update::in_flight(&self.update_state) {
            ctx.request_repaint_after(std::time::Duration::from_millis(33));
        }

        egui::TopBottomPanel::top("top").show(ctx, |ui| {
            ui.horizontal(|ui| {
                ui.heading("Synbad");
                ui.separator();
                ui.selectable_value(&mut self.tab, Tab::Status, "Dashboard");
                ui.selectable_value(&mut self.tab, Tab::Layout, "Layout");
                let peers_label = if self.discovered_peers.is_empty() {
                    "Peers".to_string()
                } else {
                    format!("Peers ({})", self.discovered_peers.len())
                };
                ui.selectable_value(&mut self.tab, Tab::Peers, peers_label);
                ui.selectable_value(&mut self.tab, Tab::Audio, "Audio");
                ui.selectable_value(&mut self.tab, Tab::Settings, "Settings");
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    let (color, text) = views::state_chip(&self.state, self.connected);
                    ui.colored_label(color, text);
                    self.draw_sync_chip(ui);
                });
            });
        });

        egui::TopBottomPanel::bottom("bottom").show(ctx, |ui| {
            ui.horizontal(|ui| {
                let online = self.connected;
                let offline_tip = "Waiting for the synbadd daemon. Actions will be available \
                                   once it's reachable.";
                // Start is always enabled — clicking it both spawns the
                // daemon (if autostart was off and it isn't running yet)
                // *and* tells it to bring up the Core. The command
                // dispatcher waits a few seconds for the daemon's socket,
                // so the user gets one-click "start sharing" instead of
                // having to wait for `connected` to flip before retrying.
                if ui.button("Start").clicked() {
                    self.ipc
                        .daemon_wanted
                        .store(true, std::sync::atomic::Ordering::Relaxed);
                    self.send(Cmd::Start);
                }
                let stop = ui
                    .add_enabled(online, egui::Button::new("Stop"))
                    .on_disabled_hover_text(offline_tip);
                if stop.clicked() {
                    self.send(Cmd::Stop);
                }
                let restart = ui
                    .add_enabled(online, egui::Button::new("Restart"))
                    .on_disabled_hover_text(offline_tip);
                if restart.clicked() {
                    self.send(Cmd::Restart);
                }
                ui.separator();
                let apply = ui.add_enabled(online && self.dirty, egui::Button::new("Apply config"));
                if apply.clicked() {
                    self.send(Cmd::SetConfig(self.config.clone()));
                    self.dirty = false;
                    self.pending_remote_config = None;
                }
                // Revert is useful both for discarding local edits AND for
                // pulling in a remote update that arrived while we were
                // editing.
                let revert_enabled = self.dirty || self.pending_remote_config.is_some();
                let revert = ui.add_enabled(revert_enabled, egui::Button::new("Revert"));
                if revert.clicked() {
                    self.dirty = false;
                    if let Some(c) = self.pending_remote_config.take() {
                        self.config = c;
                    } else {
                        self.send(Cmd::Refresh);
                    }
                }
                if self.last_error.is_some() {
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        // The "×" comes first in right-to-left layout so it
                        // sits at the far end of the bar, with the message
                        // text to its left.
                        if ui.small_button("×").on_hover_text("Dismiss").clicked() {
                            self.last_error = None;
                        }
                        if let Some(err) = &self.last_error {
                            ui.colored_label(egui::Color32::LIGHT_RED, err);
                        }
                    });
                }
            });
        });

        if self.pending_remote_config.is_some() {
            egui::TopBottomPanel::top("remote-config-banner")
                .frame(
                    egui::Frame::none()
                        .fill(egui::Color32::from_rgb(80, 60, 0))
                        .inner_margin(6.0),
                )
                .show(ctx, |ui| {
                    ui.horizontal(|ui| {
                        ui.colored_label(
                            egui::Color32::WHITE,
                            "A newer config arrived while you were editing. \
                             Apply your edits to overwrite it, or Revert to load it.",
                        );
                    });
                });
        }

        egui::CentralPanel::default().show(ctx, |ui| match self.tab {
            Tab::Status => self.draw_status(ui),
            Tab::Layout => self.draw_layout(ui),
            Tab::Peers => self.draw_peers(ui),
            Tab::Audio => self.draw_audio(ui),
            Tab::Settings => self.draw_settings(ui),
        });

        // Floating pairing dialogs — rendered last so they layer above the
        // central panel regardless of which tab is active.
        self.draw_pairing_dialogs(ctx);

        // Updates modal — same layering reason, plus it has its own state
        // machine so we always poll/draw it even when the user is on the
        // Status tab.
        if self.show_update_dialog {
            let action = update::draw_modal(
                ctx,
                &mut self.show_update_dialog,
                &self.update_state,
                env!("CARGO_PKG_VERSION"),
            );
            match action {
                update::Action::None => {}
                update::Action::Close => {
                    self.show_update_dialog = false;
                }
                update::Action::Check => {
                    self.update_state = Some(update::spawn_check(ctx));
                }
                update::Action::Install(info) => {
                    self.update_state = Some(update::spawn_install(ctx, info));
                }
            }
        }
    }
}
