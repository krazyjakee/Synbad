//! Top-level egui app: tab bar, state, IPC plumbing.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::sync::Arc;

use eframe::CreationContext;

use synbad_config::{paths, BinaryPaths, Config, NodeRole, Screen};
use synbad_ipc::{DaemonState, DiscoveredPeer, TrustedPeer};

use crate::ipc_thread::{self, Cmd, IpcHandle, Update};
use crate::layout_editor::LayoutEditor;
use crate::tray;

const LOG_CAP: usize = 1000;

#[derive(PartialEq, Eq)]
enum Tab {
    Status,
    Layout,
    Peers,
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
    ipc: IpcHandle,
    tab: Tab,

    connected: bool,
    last_error: Option<String>,
    state: DaemonState,
    log: VecDeque<String>,

    // Edited copy of the config. `dirty` tracks whether it diverges from
    // what the daemon last reported.
    config: Config,
    dirty: bool,
    // A config update that arrived from the daemon while the user had
    // unsaved local edits. Surfaced as a banner so the user knows their
    // view is stale; Revert pulls it in without round-tripping the daemon.
    pending_remote_config: Option<Config>,

    layout: LayoutEditor,
    new_screen_name: String,

    /// Names of remote screens currently handshake-confirmed by the Core.
    /// Derived from log-line parsing in the daemon — not authoritative.
    connected_peers: BTreeSet<String>,
    /// Name of the screen the cursor is currently on (server role).
    active_screen: Option<String>,

    /// LAN-discovered peers from mDNS, keyed by machine_id (sorted via
    /// BTreeMap so the GUI renders in a stable order).
    discovered_peers: BTreeMap<String, DiscoveredPeer>,
    /// Peers the user has paired with, keyed by machine_id.
    trusted_peers: BTreeMap<String, TrustedPeer>,
    /// Pairing sessions awaiting the user's accept/decline. We render one
    /// floating dialog per entry so multiple inbound requests are all
    /// visible.
    pending_pairings: Vec<PendingPairing>,
    /// Local machine UUID and short fingerprint for the "this is us"
    /// header in the Peers tab.
    local_machine_id: Option<String>,
    local_fingerprint: Option<String>,

    /// Tray installed → close button minimizes to tray instead of exiting.
    has_tray: bool,
    /// User explicitly chose Quit (via tray or future menu item). Lets us
    /// distinguish a real exit from a close-to-tray click.
    quitting: bool,
}

impl SynbadApp {
    pub fn new(cc: &CreationContext<'_>, has_tray: bool) -> Self {
        let egui_ctx = cc.egui_ctx.clone();
        let repaint: Arc<dyn Fn() + Send + Sync> =
            Arc::new(move || egui_ctx.request_repaint());
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
        }
    }

    fn drain_tray(&mut self, ctx: &egui::Context) {
        if !self.has_tray {
            return;
        }
        while let Ok(ev) = tray::menu_event_receiver().try_recv() {
            match ev.id().as_ref() {
                tray::MENU_ID_SHOW => {
                    ctx.send_viewport_cmd(egui::ViewportCommand::Visible(true));
                    ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
                }
                tray::MENU_ID_QUIT => {
                    self.quitting = true;
                    ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                }
                _ => {}
            }
        }
    }

    /// Intercept the window close to minimize-to-tray, unless the user
    /// explicitly chose Quit. Tray-less builds get default close behavior.
    fn handle_close(&self, ctx: &egui::Context) {
        if !self.has_tray || self.quitting {
            return;
        }
        if ctx.input(|i| i.viewport().close_requested()) {
            ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
            ctx.send_viewport_cmd(egui::ViewportCommand::Visible(false));
        }
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
                Update::LocalIdentity { machine_id, fingerprint } => {
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
            }
        }
    }

    fn push_log(&mut self, line: String) {
        if self.log.len() >= LOG_CAP {
            self.log.pop_front();
        }
        self.log.push_back(line);
    }

    fn send(&self, cmd: Cmd) {
        let _ = self.ipc.cmd_tx.send(cmd);
    }
}

impl eframe::App for SynbadApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.drain_ipc();
        self.drain_tray(ctx);
        self.handle_close(ctx);

        egui::TopBottomPanel::top("top").show(ctx, |ui| {
            ui.horizontal(|ui| {
                ui.heading("Synbad");
                ui.separator();
                ui.selectable_value(&mut self.tab, Tab::Status, "Status");
                ui.selectable_value(&mut self.tab, Tab::Layout, "Layout");
                let peers_label = if self.discovered_peers.is_empty() {
                    "Peers".to_string()
                } else {
                    format!("Peers ({})", self.discovered_peers.len())
                };
                ui.selectable_value(&mut self.tab, Tab::Peers, peers_label);
                ui.selectable_value(&mut self.tab, Tab::Settings, "Settings");
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    let (color, text) = state_chip(&self.state, self.connected);
                    ui.colored_label(color, text);
                });
            });
        });

        egui::TopBottomPanel::bottom("bottom").show(ctx, |ui| {
            ui.horizontal(|ui| {
                let online = self.connected;
                let offline_tip = "Waiting for the synbadd daemon. Actions will be available \
                                   once it's reachable.";
                let start = ui
                    .add_enabled(online, egui::Button::new("Start"))
                    .on_disabled_hover_text(offline_tip);
                if start.clicked() {
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
                let apply = ui
                    .add_enabled(online && self.dirty, egui::Button::new("Apply config"));
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
                        if ui
                            .small_button("×")
                            .on_hover_text("Dismiss")
                            .clicked()
                        {
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
                .frame(egui::Frame::none().fill(egui::Color32::from_rgb(80, 60, 0)).inner_margin(6.0))
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
            Tab::Settings => self.draw_settings(ui),
        });

        // Floating pairing dialogs — rendered last so they layer above the
        // central panel regardless of which tab is active.
        self.draw_pairing_dialogs(ctx);
    }
}

impl SynbadApp {
    fn draw_status(&mut self, ui: &mut egui::Ui) {
        ui.label(format!("State: {}", state_text(&self.state)));
        if let Some(active) = &self.active_screen {
            ui.label(format!("Active screen: {}", active));
        }
        ui.label(format!(
            "Connected peers: {}",
            if self.connected_peers.is_empty() {
                "none".to_string()
            } else {
                self.connected_peers.iter().cloned().collect::<Vec<_>>().join(", ")
            }
        ));
        ui.separator();
        ui.label("Core log:");
        egui::ScrollArea::vertical()
            .stick_to_bottom(true)
            .auto_shrink([false, false])
            .show(ui, |ui| {
                for line in &self.log {
                    ui.label(egui::RichText::new(line).monospace());
                }
            });
    }

    fn draw_layout(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.label("New screen:");
            ui.text_edit_singleline(&mut self.new_screen_name);
            let enabled = !self.new_screen_name.trim().is_empty()
                && !self
                    .config
                    .screens
                    .iter()
                    .any(|s| s.name == self.new_screen_name.trim());
            if ui.add_enabled(enabled, egui::Button::new("Add")).clicked() {
                let name = self.new_screen_name.trim().to_string();
                self.config.screens.push(Screen {
                    name,
                    aliases: vec![],
                    position: synbad_config::GridPosition {
                        x: ((self.config.screens.len() as i32) % 3) * 180,
                        y: ((self.config.screens.len() as i32) / 3) * 140,
                        w: 160,
                        h: 120,
                    },
                });
                self.new_screen_name.clear();
                self.dirty = true;
            }
            ui.separator();
            if ui
                .button("Reset view")
                .on_hover_text("Recenter the canvas at the origin.")
                .clicked()
            {
                self.layout.reset_view();
            }
        });
        if self.config.screens.is_empty() {
            ui.add_space(4.0);
            ui.label(
                egui::RichText::new(
                    "Add a screen above to get started. Then drag screens next to each other \
                     in the canvas — adjacent edges become input links automatically.",
                )
                .weak(),
            );
        }
        ui.separator();
        if self.layout.show(ui, &mut self.config) {
            self.dirty = true;
        }
    }

    fn draw_peers(&mut self, ui: &mut egui::Ui) {
        // Local identity header — what the user shares during pairing.
        ui.group(|ui| {
            ui.horizontal(|ui| {
                ui.label(egui::RichText::new("This machine").strong());
                if let Some(fp) = &self.local_fingerprint {
                    ui.with_layout(
                        egui::Layout::right_to_left(egui::Align::Center),
                        |ui| {
                            ui.add(
                                egui::Label::new(
                                    egui::RichText::new(fp.clone()).monospace().strong(),
                                )
                                .selectable(true),
                            );
                            ui.label("fingerprint:");
                        },
                    );
                }
            });
            if let Some(id) = &self.local_machine_id {
                ui.horizontal(|ui| {
                    ui.label("machine id:");
                    ui.add(
                        egui::Label::new(egui::RichText::new(id.clone()).monospace())
                            .selectable(true),
                    );
                });
            }
        });
        ui.add_space(8.0);
        ui.horizontal(|ui| {
            ui.heading("Discovered peers");
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui.button("Refresh").clicked() {
                    self.send(Cmd::RefreshPeers);
                }
            });
        });

        // Collect actions to apply after the immutable iteration finishes;
        // dispatching inline would borrow `self` twice.
        let mut pair_request: Option<String> = None;
        let mut revoke_request: Option<String> = None;

        if self.discovered_peers.is_empty() {
            ui.add_space(8.0);
            ui.label(
                "No peers on the LAN yet. Start synbadd on another machine in the same \
                 subnet and it should appear here within a few seconds.",
            );
        } else {
            egui::ScrollArea::vertical()
                .auto_shrink([false, true])
                .show(ui, |ui| {
                    egui::Grid::new("peers-grid")
                        .num_columns(6)
                        .striped(true)
                        .spacing([12.0, 4.0])
                        .show(ui, |ui| {
                            ui.label(egui::RichText::new("Name").strong());
                            ui.label(egui::RichText::new("Host").strong());
                            ui.label(egui::RichText::new("Ports").strong());
                            ui.label(egui::RichText::new("Fingerprint").strong());
                            ui.label(egui::RichText::new("Trust").strong());
                            ui.label("");
                            ui.end_row();

                            for peer in self.discovered_peers.values() {
                                let is_trusted =
                                    self.trusted_peers.contains_key(&peer.machine_id);
                                ui.label(&peer.display_name);
                                ui.add(
                                    egui::Label::new(
                                        egui::RichText::new(&peer.host).monospace(),
                                    )
                                    .selectable(true),
                                );
                                ui.label(format!(
                                    "synbad:{} core:{}",
                                    peer.service_port, peer.core_port
                                ));
                                ui.add(
                                    egui::Label::new(
                                        egui::RichText::new(&peer.fingerprint).monospace(),
                                    )
                                    .selectable(true),
                                );
                                if is_trusted {
                                    ui.colored_label(egui::Color32::LIGHT_GREEN, "trusted");
                                } else {
                                    ui.colored_label(
                                        egui::Color32::LIGHT_YELLOW,
                                        "unverified",
                                    );
                                }
                                if is_trusted {
                                    if ui.button("Revoke").clicked() {
                                        revoke_request = Some(peer.machine_id.clone());
                                    }
                                } else if ui.button("Pair").clicked() {
                                    pair_request = Some(peer.machine_id.clone());
                                }
                                ui.end_row();
                            }
                        });
                });
        }

        if let Some(id) = pair_request {
            self.send(Cmd::StartPairing { machine_id: id });
        }
        if let Some(id) = revoke_request {
            self.send(Cmd::RevokeTrust { machine_id: id });
        }

        // Trusted peers that aren't currently visible on mDNS — still show
        // them so the user can revoke without waiting for the peer to come
        // back online.
        let offline_trusted: Vec<_> = self
            .trusted_peers
            .values()
            .filter(|t| !self.discovered_peers.contains_key(&t.machine_id))
            .cloned()
            .collect();
        if !offline_trusted.is_empty() {
            ui.add_space(12.0);
            ui.heading("Trusted (offline)");
            let mut revoke_offline: Option<String> = None;
            egui::Grid::new("trusted-offline-grid")
                .num_columns(3)
                .striped(true)
                .spacing([12.0, 4.0])
                .show(ui, |ui| {
                    for t in &offline_trusted {
                        ui.label(&t.display_name);
                        ui.add(
                            egui::Label::new(
                                egui::RichText::new(&t.fingerprint).monospace(),
                            )
                            .selectable(true),
                        );
                        if ui.button("Revoke").clicked() {
                            revoke_offline = Some(t.machine_id.clone());
                        }
                        ui.end_row();
                    }
                });
            if let Some(id) = revoke_offline {
                self.send(Cmd::RevokeTrust { machine_id: id });
            }
        }
    }

    /// Render a floating window for each pending pairing session. The
    /// window shows the SAS verification code and Accept/Decline buttons.
    fn draw_pairing_dialogs(&mut self, ctx: &egui::Context) {
        let mut decisions: Vec<(String, bool)> = Vec::new();
        for (i, pending) in self.pending_pairings.iter().enumerate() {
            // Unique window ID per session so multiple inbound proposals
            // are each rendered.
            let id = egui::Id::new(("pair-dialog", &pending.session_id));
            // Offset stacked windows so they don't perfectly overlap.
            let offset = i as f32 * 24.0;
            egui::Window::new(format!("Pair with {}", pending.peer_display_name))
                .id(id)
                .collapsible(false)
                .resizable(false)
                .default_pos(ctx.screen_rect().center() + egui::Vec2::new(offset, offset))
                .anchor(egui::Align2::CENTER_CENTER, egui::Vec2::new(offset, offset))
                .show(ctx, |ui| {
                    ui.label(
                        "Compare the verification code on both machines. \
                         Only accept if the codes match.",
                    );
                    ui.add_space(8.0);
                    ui.vertical_centered(|ui| {
                        ui.label(
                            egui::RichText::new(&pending.verification_code)
                                .heading()
                                .monospace()
                                .strong()
                                .color(egui::Color32::LIGHT_BLUE),
                        );
                    });
                    ui.add_space(8.0);
                    ui.label(format!(
                        "Peer machine: {}",
                        pending.peer_machine_id
                    ));
                    ui.label(format!(
                        "Peer fingerprint: {}",
                        pending.peer_fingerprint
                    ));
                    ui.add_space(12.0);
                    ui.horizontal(|ui| {
                        if ui
                            .add(
                                egui::Button::new("Decline")
                                    .fill(egui::Color32::DARK_RED),
                            )
                            .clicked()
                        {
                            decisions.push((pending.session_id.clone(), false));
                        }
                        if ui
                            .add(
                                egui::Button::new("Accept")
                                    .fill(egui::Color32::DARK_GREEN),
                            )
                            .clicked()
                        {
                            decisions.push((pending.session_id.clone(), true));
                        }
                    });
                    ui.add_space(2.0);
                    ui.label(
                        egui::RichText::new("Press Esc to decline.")
                            .small()
                            .weak(),
                    );
                });
        }
        // Esc declines the topmost pending dialog. We deliberately do NOT
        // bind Enter to Accept — accepting must be an explicit click, since
        // this is the moment the user verifies the SAS code.
        if !self.pending_pairings.is_empty()
            && ctx.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::Escape))
        {
            if let Some(p) = self.pending_pairings.last() {
                decisions.push((p.session_id.clone(), false));
            }
        }
        for (session_id, accept) in decisions {
            self.pending_pairings.retain(|p| p.session_id != session_id);
            self.send(Cmd::ConfirmPairing { session_id, accept });
        }
    }

    fn draw_settings(&mut self, ui: &mut egui::Ui) {
        egui::Grid::new("settings").num_columns(2).show(ui, |ui| {
            ui.label("Role");
            ui.horizontal(|ui| {
                let mut role = self.config.role;
                if ui.radio_value(&mut role, NodeRole::Server, "Server").changed()
                    || ui.radio_value(&mut role, NodeRole::Client, "Client").changed()
                {
                    self.config.role = role;
                    self.dirty = true;
                }
            });
            ui.end_row();

            ui.label("This machine's name");
            let name_resp = ui.text_edit_singleline(&mut self.config.server_name);
            if name_resp.changed() {
                self.dirty = true;
            }
            name_resp.on_hover_text(
                "The screen name this machine advertises (becomes `computerName` in \
                 deskflow.ini).",
            );
            ui.end_row();

            ui.label("Port");
            let mut port = self.config.port as i32;
            if ui.add(egui::DragValue::new(&mut port).clamp_range(1..=65535)).changed() {
                self.config.port = port.clamp(1, 65535) as u16;
                self.dirty = true;
            }
            ui.end_row();

            let client_mode = matches!(self.config.role, NodeRole::Client);
            ui.label("Server address");
            ui.horizontal(|ui| {
                let mut addr = self.config.server_address.clone().unwrap_or_default();
                let resp = ui.add_enabled(
                    client_mode,
                    egui::TextEdit::singleline(&mut addr)
                        .hint_text("host or host:port (client mode only)"),
                );
                if resp.changed() {
                    self.config.server_address =
                        if addr.is_empty() { None } else { Some(addr) };
                    self.dirty = true;
                }
                if !client_mode {
                    resp.on_disabled_hover_text(
                        "Only used in Client mode — switch the role above to enable.",
                    );
                }
            });
            ui.end_row();

            ui.label("deskflow-core path (override)");
            let mut core_bin = self
                .config
                .binaries
                .core
                .as_ref()
                .map(|p| p.display().to_string())
                .unwrap_or_default();
            let resp = ui.text_edit_singleline(&mut core_bin);
            if resp.changed() {
                self.config.binaries = BinaryPaths { core: opt_path(&core_bin) };
                self.dirty = true;
            }
            resp.on_hover_text(
                "Leave blank to fetch the latest deskflow-core from \
                 github.com/deskflow/deskflow on first start.",
            );
            ui.end_row();
        });

        ui.separator();
        ui.collapsing("Generated synergy.conf preview", |ui| {
            let preview = self.config.generate_synergy_conf();
            ui.add(
                egui::TextEdit::multiline(&mut preview.as_str())
                    .font(egui::TextStyle::Monospace)
                    .desired_rows(12)
                    .desired_width(f32::INFINITY),
            );
        });
        ui.collapsing("Generated deskflow.ini preview", |ui| {
            let preview = self
                .config
                .generate_deskflow_settings(&synbad_config::paths::generated_conf());
            ui.add(
                egui::TextEdit::multiline(&mut preview.as_str())
                    .font(egui::TextStyle::Monospace)
                    .desired_rows(10)
                    .desired_width(f32::INFINITY),
            );
        });
    }
}

fn opt_path(s: &str) -> Option<std::path::PathBuf> {
    let t = s.trim();
    if t.is_empty() {
        None
    } else {
        Some(std::path::PathBuf::from(t))
    }
}

fn state_chip(s: &DaemonState, connected: bool) -> (egui::Color32, String) {
    if !connected {
        return (egui::Color32::GRAY, "daemon offline".into());
    }
    match s {
        DaemonState::Stopped => (egui::Color32::GRAY, "stopped".into()),
        DaemonState::Starting => (egui::Color32::YELLOW, "starting".into()),
        DaemonState::Running { pid } => {
            (egui::Color32::LIGHT_GREEN, format!("running (pid {})", pid))
        }
        DaemonState::Crashed { exit_code } => (
            egui::Color32::LIGHT_RED,
            format!("crashed (exit {:?})", exit_code),
        ),
    }
}

fn state_text(s: &DaemonState) -> String {
    match s {
        DaemonState::Stopped => "stopped".into(),
        DaemonState::Starting => "starting".into(),
        DaemonState::Running { pid } => format!("running (pid {})", pid),
        DaemonState::Crashed { exit_code } => format!("crashed (exit {:?})", exit_code),
    }
}
