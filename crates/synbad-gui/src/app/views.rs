//! Per-tab rendering and the small UI helpers shared by the top bar.
//!
//! Every `draw_*` here is called from `SynbadApp::update` in [`super`];
//! they read from the cached state owned by [`SynbadApp`] and either
//! mutate the edit buffer (`config`, `dirty`) or push a `Cmd` onto the
//! IPC channel. No long-lived state lives in this file — by design.

use synbad_config::{BinaryPaths, NodeRole, Screen};
use synbad_ipc::{AudioDeviceInfo, DaemonState, DiscoveredPeer};

use crate::ipc_thread::Cmd;

use super::SynbadApp;

impl SynbadApp {
    /// Top-bar indicator for config-sync activity. Shows "syncing N peer(s)"
    /// while at least one session is active, otherwise the last completed
    /// outcome for a few seconds, then disappears. Without this, the user
    /// has no way to know whether their layout edits actually reached the
    /// other side.
    pub(super) fn draw_sync_chip(&mut self, ui: &mut egui::Ui) {
        // Auto-expire stale chips so the bar doesn't permanently carry the
        // last sync result from an hour ago.
        const STATUS_TTL_SECS: u64 = 6;
        if let Some(s) = &self.last_sync_status {
            if s.at.elapsed().as_secs() >= STATUS_TTL_SECS {
                self.last_sync_status = None;
            }
        }

        if !self.active_syncs.is_empty() {
            ui.separator();
            let label = if self.active_syncs.len() == 1 {
                let (id, _) = self.active_syncs.iter().next().unwrap();
                format!("syncing with {}", self.peer_label(id))
            } else {
                format!("syncing {} peers", self.active_syncs.len())
            };
            ui.colored_label(egui::Color32::LIGHT_BLUE, label);
        } else if let Some(status) = self.last_sync_status.clone() {
            ui.separator();
            let color = if status.ok {
                egui::Color32::LIGHT_GREEN
            } else {
                egui::Color32::LIGHT_RED
            };
            ui.colored_label(color, status.message);
        }
    }

    pub(super) fn draw_status(&mut self, ui: &mut egui::Ui) {
        // Hero summary — single glanceable line that tells the user
        // whether everything's working. Derived from the same state the
        // cards below break down. Colour matches the top-bar chip so the
        // signal is consistent across the window.
        let health = self.overall_health();
        ui.add_space(2.0);
        ui.horizontal(|ui| {
            ui.label(
                egui::RichText::new(health.icon)
                    .heading()
                    .color(health.colour),
            );
            ui.vertical(|ui| {
                ui.label(
                    egui::RichText::new(health.headline)
                        .heading()
                        .color(health.colour),
                );
                if let Some(detail) = health.detail.as_deref() {
                    ui.label(egui::RichText::new(detail).weak());
                }
            });
        });
        ui.add_space(8.0);

        // Two columns of cards. Re-flowed by hand instead of using a grid
        // so each card can grow vertically without dragging the others
        // along with it.
        egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            .show(ui, |ui| {
                ui.columns(2, |cols| {
                    self.draw_connection_card(&mut cols[0]);
                    self.draw_sharing_card(&mut cols[1]);
                });
                ui.add_space(8.0);
                ui.columns(2, |cols| {
                    self.draw_peers_card(&mut cols[0]);
                    self.draw_audio_card(&mut cols[1]);
                });
                ui.add_space(8.0);
                self.draw_sync_card(ui);

                // The raw Core log is a technical aid — keep it out of
                // the dashboard by default. Advanced mode pops it back
                // in at the bottom.
                if self.show_advanced {
                    ui.add_space(12.0);
                    ui.separator();
                    ui.collapsing("Core log", |ui| {
                        egui::ScrollArea::vertical()
                            .id_source("core-log-scroll")
                            .stick_to_bottom(true)
                            .max_height(220.0)
                            .auto_shrink([false, false])
                            .show(ui, |ui| {
                                for line in &self.log {
                                    ui.label(egui::RichText::new(line).monospace().small());
                                }
                            });
                    });
                }
            });
    }

    fn draw_connection_card(&self, ui: &mut egui::Ui) {
        card(ui, "Connection", |ui| {
            let wants_daemon = self
                .ipc
                .daemon_wanted
                .load(std::sync::atomic::Ordering::Relaxed);
            let (daemon_colour, daemon_text) = if self.connected {
                (egui::Color32::LIGHT_GREEN, "Daemon connected")
            } else if wants_daemon {
                (egui::Color32::LIGHT_RED, "Daemon offline")
            } else {
                (egui::Color32::GRAY, "Daemon not started")
            };
            ui.horizontal(|ui| {
                ui.label(egui::RichText::new("●").color(daemon_colour));
                ui.label(daemon_text);
            });
            let (core_colour, core_text) = match &self.state {
                DaemonState::Running { pid } => (
                    egui::Color32::LIGHT_GREEN,
                    format!("Core running (pid {pid})"),
                ),
                DaemonState::Starting => (egui::Color32::YELLOW, "Core starting…".into()),
                DaemonState::Stopped => (egui::Color32::GRAY, "Core stopped".into()),
                DaemonState::Crashed { exit_code } => (
                    egui::Color32::LIGHT_RED,
                    format!("Core crashed (exit {exit_code:?})"),
                ),
            };
            ui.horizontal(|ui| {
                ui.label(egui::RichText::new("●").color(core_colour));
                ui.label(core_text);
            });
            if let Some(err) = &self.last_error {
                ui.add_space(2.0);
                ui.label(
                    egui::RichText::new(err)
                        .small()
                        .color(egui::Color32::LIGHT_RED),
                );
            }
        });
    }

    fn draw_sharing_card(&self, ui: &mut egui::Ui) {
        card(ui, "Sharing", |ui| {
            let role_text = match self.config.role {
                NodeRole::Server => "Server (this machine drives input)",
                NodeRole::Client => "Client (driven by another machine)",
            };
            ui.label(role_text);
            if let Some(active) = &self.active_screen {
                ui.horizontal(|ui| {
                    ui.label("Active screen:");
                    ui.label(egui::RichText::new(active).strong());
                });
            }
            let linked = self.connected_peers.len();
            ui.label(format!(
                "{} {} linked",
                linked,
                if linked == 1 { "peer" } else { "peers" }
            ));
        });
    }

    fn draw_peers_card(&self, ui: &mut egui::Ui) {
        card(ui, "Peers", |ui| {
            let paired = self.trusted_peers.len();
            let online = self
                .trusted_peers
                .keys()
                .filter(|id| self.discovered_peers.contains_key(*id))
                .count();
            let unverified = self
                .discovered_peers
                .iter()
                .filter(|(id, _)| !self.trusted_peers.contains_key(*id))
                .count();
            ui.label(format!("{} paired ({} online)", paired, online,));
            if unverified > 0 {
                ui.label(
                    egui::RichText::new(format!(
                        "{} unpaired on LAN — Peers tab to pair",
                        unverified
                    ))
                    .color(egui::Color32::LIGHT_YELLOW),
                );
            } else if paired == 0 {
                ui.label(
                    egui::RichText::new("No peers yet. Start synbad on another machine.").weak(),
                );
            }
        });
    }

    fn draw_audio_card(&self, ui: &mut egui::Ui) {
        card(ui, "Audio", |ui| {
            if !self.config.audio.enabled {
                ui.label(egui::RichText::new("Off").weak());
                ui.label(
                    egui::RichText::new("Enable in the Audio tab to stream mic / system audio.")
                        .small()
                        .weak(),
                );
                return;
            }
            let streaming = self
                .audio_peer_status
                .values()
                .filter(|s| s.sending_to_peer || s.receiving_from_peer)
                .count();
            if streaming == 0 {
                ui.label("On — no active streams");
            } else {
                ui.label(
                    egui::RichText::new(format!(
                        "{} stream{} up",
                        streaming,
                        if streaming == 1 { "" } else { "s" }
                    ))
                    .color(egui::Color32::LIGHT_GREEN),
                );
            }
            if let Some(err) = &self.audio_last_error {
                ui.label(
                    egui::RichText::new(format!("⚠ {err}"))
                        .small()
                        .color(egui::Color32::LIGHT_RED),
                );
            }
        });
    }

    fn draw_sync_card(&self, ui: &mut egui::Ui) {
        card(ui, "Layout sync", |ui| {
            if !self.active_syncs.is_empty() {
                ui.label(
                    egui::RichText::new(format!(
                        "Syncing with {} peer{}…",
                        self.active_syncs.len(),
                        if self.active_syncs.len() == 1 {
                            ""
                        } else {
                            "s"
                        }
                    ))
                    .color(egui::Color32::LIGHT_BLUE),
                );
            } else if let Some(s) = &self.last_sync_status {
                let colour = if s.ok {
                    egui::Color32::LIGHT_GREEN
                } else {
                    egui::Color32::LIGHT_RED
                };
                ui.label(egui::RichText::new(&s.message).color(colour));
            } else if self.trusted_peers.is_empty() {
                ui.label(egui::RichText::new("Pair a peer to start syncing layout edits.").weak());
            } else {
                ui.label(egui::RichText::new("Idle — layout is in sync.").weak());
            }
        });
    }

    /// Roll the per-card signals up into a single hero state. The
    /// precedence is: connection failures first (you can't tell anything
    /// is working without a daemon), then Core crash, then audio errors,
    /// then "all good". The detail string is intentionally short — it
    /// shows next to the icon, so it has to fit on one line.
    fn overall_health(&self) -> HealthSummary {
        if !self.connected {
            // Two distinct "offline" stories: the user has autostart off
            // and just hasn't clicked Start yet (calm idle state) vs. we
            // *wanted* the daemon up and can't reach it (something's
            // wrong). `daemon_wanted` is the session flag the event loop
            // is gated on, so it captures both initial autostart and a
            // Start click that happened mid-session.
            let wants_daemon = self
                .ipc
                .daemon_wanted
                .load(std::sync::atomic::Ordering::Relaxed);
            if !wants_daemon {
                return HealthSummary {
                    icon: "●",
                    colour: egui::Color32::GRAY,
                    headline: "Not started",
                    detail: Some("Click Start to launch synbadd and begin sharing.".into()),
                };
            }
            return HealthSummary {
                icon: "●",
                colour: egui::Color32::LIGHT_RED,
                headline: "Daemon offline",
                detail: Some(
                    "Synbad is trying to reconnect — the rest of the UI will catch up \
                     once it's reachable."
                        .into(),
                ),
            };
        }
        if let DaemonState::Crashed { exit_code } = &self.state {
            return HealthSummary {
                icon: "●",
                colour: egui::Color32::LIGHT_RED,
                headline: "Core crashed",
                detail: Some(format!(
                    "Last exit code: {:?}. See the log in advanced mode for details.",
                    exit_code
                )),
            };
        }
        if let Some(err) = &self.audio_last_error {
            return HealthSummary {
                icon: "⚠",
                colour: egui::Color32::LIGHT_YELLOW,
                headline: "Audio bridge degraded",
                detail: Some(err.clone()),
            };
        }
        match &self.state {
            DaemonState::Starting => HealthSummary {
                icon: "●",
                colour: egui::Color32::YELLOW,
                headline: "Core starting…",
                detail: None,
            },
            DaemonState::Stopped => HealthSummary {
                icon: "●",
                colour: egui::Color32::GRAY,
                headline: "Idle",
                detail: Some("Click Start to begin sharing input.".into()),
            },
            DaemonState::Running { .. } => HealthSummary {
                icon: "●",
                colour: egui::Color32::LIGHT_GREEN,
                headline: "Everything's working",
                detail: None,
            },
            DaemonState::Crashed { .. } => unreachable!("handled above"),
        }
    }

    pub(super) fn draw_layout(&mut self, ui: &mut egui::Ui) {
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
                    monitors: vec![],
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

    pub(super) fn draw_peers(&mut self, ui: &mut egui::Ui) {
        // Local identity header — what the user shares during pairing.
        ui.group(|ui| {
            ui.horizontal(|ui| {
                ui.label(egui::RichText::new("This machine").strong());
                if let Some(fp) = &self.local_fingerprint {
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        ui.add(
                            egui::Label::new(egui::RichText::new(fp.clone()).monospace().strong())
                                .selectable(true),
                        );
                        ui.label("fingerprint:");
                    });
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
        let mut use_as_server: Option<DiscoveredPeer> = None;

        if self.discovered_peers.is_empty() {
            ui.add_space(8.0);
            ui.label(
                "No peers on the LAN yet. Start synbadd on another machine in the same \
                 subnet and it should appear here within a few seconds.",
            );
        } else {
            // Pre-compute which peer (if any) is the current server target,
            // so we can render a "connected" badge instead of a button on
            // that row.
            let configured_server_host = self
                .config
                .server_address
                .as_deref()
                .map(|s| s.split(':').next().unwrap_or(s).to_string());
            let local_is_client = matches!(self.config.role, NodeRole::Client);

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
                                let is_trusted = self.trusted_peers.contains_key(&peer.machine_id);
                                let is_active_server = local_is_client
                                    && configured_server_host
                                        .as_deref()
                                        .map(|h| h == peer.host)
                                        .unwrap_or(false);
                                ui.label(&peer.display_name);
                                ui.add(
                                    egui::Label::new(egui::RichText::new(&peer.host).monospace())
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
                                    ui.colored_label(egui::Color32::LIGHT_YELLOW, "unverified");
                                }
                                ui.horizontal(|ui| {
                                    if is_trusted {
                                        if is_active_server {
                                            ui.colored_label(
                                                egui::Color32::LIGHT_GREEN,
                                                "← server",
                                            );
                                        } else {
                                            // Connecting needs a Core port; an unstarted
                                            // peer advertises 0 and we can't dial it.
                                            let has_core = peer.core_port != 0;
                                            let btn = ui.add_enabled(
                                                has_core,
                                                egui::Button::new("Use as server"),
                                            );
                                            if !has_core {
                                                btn.on_disabled_hover_text(
                                                    "Peer's Synergy Core isn't running yet. \
                                                     Start it on the other machine first.",
                                                );
                                            } else if btn.clicked() {
                                                use_as_server = Some(peer.clone());
                                            }
                                        }
                                        if ui.button("Revoke").clicked() {
                                            revoke_request = Some(peer.machine_id.clone());
                                        }
                                    } else if ui.button("Pair").clicked() {
                                        pair_request = Some(peer.machine_id.clone());
                                    }
                                });
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
        if let Some(peer) = use_as_server {
            self.connect_as_client_to(&peer);
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
                            egui::Label::new(egui::RichText::new(&t.fingerprint).monospace())
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
    pub(super) fn draw_pairing_dialogs(&mut self, ctx: &egui::Context) {
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
                    ui.label(format!("Peer machine: {}", pending.peer_machine_id));
                    ui.label(format!("Peer fingerprint: {}", pending.peer_fingerprint));
                    ui.add_space(12.0);
                    ui.horizontal(|ui| {
                        if ui
                            .add(egui::Button::new("Decline").fill(egui::Color32::DARK_RED))
                            .clicked()
                        {
                            decisions.push((pending.session_id.clone(), false));
                        }
                        if ui
                            .add(egui::Button::new("Accept").fill(egui::Color32::DARK_GREEN))
                            .clicked()
                        {
                            decisions.push((pending.session_id.clone(), true));
                        }
                    });
                    ui.add_space(2.0);
                    ui.label(egui::RichText::new("Press Esc to decline.").small().weak());
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

    pub(super) fn draw_audio(&mut self, ui: &mut egui::Ui) {
        // Lazy-load the device list when the tab is first opened so we
        // don't probe cpal until the user actually wants it. Also pull a
        // fresh status snapshot so the per-peer table isn't stale from
        // before we joined this tab.
        if !self.audio_devices_loaded {
            self.send(Cmd::ListAudioDevices);
            self.send(Cmd::GetAudioStatus);
            self.audio_devices_loaded = true;
        }

        ui.horizontal(|ui| {
            ui.heading("Audio bridge");
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui.small_button("Refresh devices").clicked() {
                    self.send(Cmd::ListAudioDevices);
                }
            });
        });
        ui.label(
            "Streams microphone audio bidirectionally between paired peers \
             over WebRTC. Flip Enabled on both ends to start streaming.",
        );

        if let Some(err) = self.audio_last_error.clone() {
            ui.horizontal(|ui| {
                ui.colored_label(egui::Color32::LIGHT_RED, format!("⚠ {err}"));
                // The capture-side LoopbackUnavailable error has no
                // actionable fix without a virtual audio device — drop
                // a link to BlackHole right next to the message so the
                // user knows what to install rather than guessing.
                if err.contains("loopback capture not available")
                    || err.contains("virtual audio device")
                {
                    ui.hyperlink_to("Install BlackHole", "https://existential.audio/blackhole/");
                }
                if ui.small_button("Dismiss").clicked() {
                    self.audio_last_error = None;
                }
            });
        }

        ui.separator();

        let mut audio = self.config.audio.clone();
        let mut audio_changed = false;

        egui::Grid::new("audio-settings")
            .num_columns(2)
            .show(ui, |ui| {
                ui.label("Enabled");
                if ui
                    .checkbox(&mut audio.enabled, "")
                    .on_hover_text(
                        "Master switch. When on, every paired peer gets a \
                         bidirectional audio session. Per-peer overrides \
                         can mute one direction or a specific link.",
                    )
                    .changed()
                {
                    audio_changed = true;
                }
                ui.end_row();

                // Device pickers are no-ops with the master switch off;
                // gray them out so the disabled state matches the runtime.
                let active = audio.enabled;

                ui.label("Input device");
                ui.add_enabled_ui(active, |ui| {
                    if device_combo(
                        ui,
                        "audio-input",
                        &self.audio_input_devices,
                        &mut audio.input_device,
                    ) {
                        audio_changed = true;
                    }
                });
                ui.end_row();

                ui.label("Output device");
                ui.add_enabled_ui(active, |ui| {
                    if device_combo(
                        ui,
                        "audio-output",
                        &self.audio_output_devices,
                        &mut audio.output_device,
                    ) {
                        audio_changed = true;
                    }
                });
                ui.end_row();

                // Linear sliders, 0..=2.0. Unity is 100 %, 0 % mutes,
                // 200 % is +6 dB (clamped at i16 saturation so loud
                // sources clip rather than wrap). Matches the
                // device-picker behaviour: change applies immediately
                // and persists via SetAudioConfig.
                ui.label("Input volume");
                ui.add_enabled_ui(active, |ui| {
                    if ui
                        .add(
                            egui::Slider::new(&mut audio.input_gain, 0.0..=2.0)
                                .custom_formatter(|v, _| format!("{:.0}%", v * 100.0))
                                .custom_parser(|s| {
                                    s.trim_end_matches('%')
                                        .parse::<f64>()
                                        .ok()
                                        .map(|n| n / 100.0)
                                }),
                        )
                        .on_hover_text(
                            "Microphone gain. 100% is unity; values above 100% \
                             boost the signal but will clip loud sources.",
                        )
                        .changed()
                    {
                        audio_changed = true;
                    }
                });
                ui.end_row();

                ui.label("Output volume");
                ui.add_enabled_ui(active, |ui| {
                    if ui
                        .add(
                            egui::Slider::new(&mut audio.output_gain, 0.0..=2.0)
                                .custom_formatter(|v, _| format!("{:.0}%", v * 100.0))
                                .custom_parser(|s| {
                                    s.trim_end_matches('%')
                                        .parse::<f64>()
                                        .ok()
                                        .map(|n| n / 100.0)
                                }),
                        )
                        .on_hover_text(
                            "Playback gain applied to received peer audio. \
                             100% is unity; values above clip on loud peers.",
                        )
                        .changed()
                    {
                        audio_changed = true;
                    }
                });
                ui.end_row();

                ui.label("Signaling port");
                let mut port = audio.signal_port as i32;
                if ui
                    .add(egui::DragValue::new(&mut port).clamp_range(1..=65535))
                    .changed()
                {
                    audio.signal_port = port.clamp(1, 65535) as u16;
                    audio_changed = true;
                }
                ui.end_row();
            });

        if audio_changed {
            // Audio is pushed and persisted immediately, so it doesn't
            // participate in the dirty/Apply flow — flipping `dirty` here
            // would light up Apply/Revert and the "remote config" banner
            // for a change the daemon has already saved.
            self.config.audio = audio.clone();
            self.send(Cmd::SetAudioConfig(audio));
        }

        ui.separator();
        ui.heading("Per-peer status");
        if self.audio_peer_status.is_empty() {
            ui.label("No active audio sessions.");
        } else {
            // RTT is plumbed through the IPC type but the bridge doesn't
            // surface RTCP receiver-report numbers yet, so it's omitted
            // here to avoid a column that's always blank. Glyphs are
            // plain ASCII so they render without depending on whatever
            // unicode coverage the system font ships with.
            egui::Grid::new("audio-peer-status")
                .num_columns(4)
                .striped(true)
                .show(ui, |ui| {
                    ui.label(egui::RichText::new("Peer").strong());
                    ui.label(egui::RichText::new("Send").strong());
                    ui.label(egui::RichText::new("Receive").strong());
                    ui.label(egui::RichText::new("State").strong());
                    ui.end_row();
                    for status in self.audio_peer_status.values() {
                        ui.label(&status.display_name);
                        direction_cell(ui, status.sending_to_peer);
                        direction_cell(ui, status.receiving_from_peer);
                        match &status.last_error {
                            Some(err) => {
                                ui.colored_label(egui::Color32::LIGHT_RED, err);
                            }
                            None if status.sending_to_peer || status.receiving_from_peer => {
                                ui.colored_label(egui::Color32::LIGHT_GREEN, "connected");
                            }
                            None => {
                                ui.label("negotiating…");
                            }
                        }
                        ui.end_row();
                    }
                });
        }
    }

    pub(super) fn draw_settings(&mut self, ui: &mut egui::Ui) {
        // Visible-by-default settings. Deliberately small — the GUI is
        // meant to be turnkey, so the only knob most users ever need is
        // whether the daemon also boots when the window opens. Everything
        // else lives behind "Show advanced".
        let mut autostart = self.config.autostart;
        let resp = ui.checkbox(&mut autostart, "Autostart synbadd with this app");
        if resp.changed() {
            self.config.autostart = autostart;
            self.dirty = true;
            // The user just told us their preference for the *startup*
            // path — propagate it to the live session flag so the event
            // loop's reconnect/respawn logic picks it up immediately
            // without waiting for the next GUI launch.
            self.ipc
                .daemon_wanted
                .store(autostart, std::sync::atomic::Ordering::Relaxed);
        }
        resp.on_hover_text(
            "On (default): synbadd launches when this app opens. \
             Off: synbadd stays down until you click Start. Either way the GUI \
             owns the daemon's lifecycle — closing the window stops it.",
        );

        ui.add_space(8.0);
        ui.horizontal(|ui| {
            ui.label(format!("Synbad version {}", env!("CARGO_PKG_VERSION")));
            if ui.button("Check for updates…").clicked() {
                self.open_update_dialog(ui.ctx());
            }
        });

        ui.add_space(8.0);
        ui.checkbox(&mut self.show_advanced, "Show advanced settings")
            .on_hover_text(
                "Reveals the underlying Deskflow Core settings (role, ports, server \
                 address, binary override) and the generated config previews. Most \
                 users never need this — pairing fills in the role and address \
                 automatically.",
            );

        if !self.show_advanced {
            return;
        }

        ui.add_space(8.0);
        ui.separator();
        ui.label(
            egui::RichText::new("Advanced")
                .strong()
                .color(egui::Color32::LIGHT_GRAY),
        );
        ui.add_space(4.0);

        egui::Grid::new("settings-advanced")
            .num_columns(2)
            .show(ui, |ui| {
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

                ui.label("Share clipboard");
                let mut share = self.config.clipboard_sharing;
                let resp = ui.checkbox(&mut share, "");
                if resp.changed() {
                    self.config.clipboard_sharing = share;
                    self.dirty = true;
                }
                resp.on_hover_text(
                    "When off, the Synergy Core won't relay clipboard contents between \
                     machines (emits `clipboardSharing = false` into synergy.conf).",
                );
                ui.end_row();

                ui.label("Role");
                ui.horizontal(|ui| {
                    let mut role = self.config.role;
                    if ui
                        .radio_value(&mut role, NodeRole::Server, "Server")
                        .changed()
                        || ui
                            .radio_value(&mut role, NodeRole::Client, "Client")
                            .changed()
                    {
                        self.config.role = role;
                        self.dirty = true;
                    }
                })
                .response
                .on_hover_text(
                    "Normally set by the pairing flow — click 'Use as server' in the \
                     Peers tab and we'll switch to Client mode for you.",
                );
                ui.end_row();

                ui.label("Port");
                let mut port = self.config.port as i32;
                if ui
                    .add(egui::DragValue::new(&mut port).clamp_range(1..=65535))
                    .changed()
                {
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
                    self.config.binaries = BinaryPaths {
                        core: opt_path(&core_bin),
                    };
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

/// One-line dashboard hero summary. Built by [`SynbadApp::overall_health`].
struct HealthSummary {
    icon: &'static str,
    colour: egui::Color32,
    headline: &'static str,
    detail: Option<String>,
}

/// Render a titled card with a subtle border. Used for the dashboard
/// status tiles so each signal has its own visual chunk instead of
/// running together as a wall of labels.
fn card(ui: &mut egui::Ui, title: &str, body: impl FnOnce(&mut egui::Ui)) {
    egui::Frame::group(ui.style())
        .inner_margin(egui::Margin::same(10.0))
        .show(ui, |ui| {
            ui.set_min_width(ui.available_width());
            ui.label(
                egui::RichText::new(title)
                    .small()
                    .color(egui::Color32::LIGHT_GRAY)
                    .strong(),
            );
            ui.add_space(4.0);
            body(ui);
        });
}

fn opt_path(s: &str) -> Option<std::path::PathBuf> {
    let t = s.trim();
    if t.is_empty() {
        None
    } else {
        Some(std::path::PathBuf::from(t))
    }
}

/// Combo-box of audio devices. `selected` holds the chosen device name or
/// `None` for "OS default." Returns `true` if the selection changed.
fn device_combo(
    ui: &mut egui::Ui,
    id: &str,
    devices: &[AudioDeviceInfo],
    selected: &mut Option<String>,
) -> bool {
    let mut changed = false;
    let label = selected
        .clone()
        .unwrap_or_else(|| "(OS default)".to_string());
    egui::ComboBox::from_id_source(id)
        .selected_text(label)
        .show_ui(ui, |ui| {
            if ui
                .selectable_value(selected, None, "(OS default)")
                .clicked()
            {
                changed = true;
            }
            for dev in devices {
                let mut display = dev.name.clone();
                if dev.is_loopback {
                    display.push_str("  (loopback)");
                }
                if dev.is_default {
                    display.push_str("  *");
                }
                if ui
                    .selectable_value(selected, Some(dev.name.clone()), display)
                    .clicked()
                {
                    changed = true;
                }
            }
        });
    changed
}

/// Render an "on / off" cell for the per-peer audio direction columns.
/// Plain ASCII so the result is identical regardless of the system font.
fn direction_cell(ui: &mut egui::Ui, on: bool) {
    if on {
        ui.colored_label(egui::Color32::LIGHT_GREEN, "on");
    } else {
        ui.colored_label(egui::Color32::GRAY, "off");
    }
}

pub(super) fn state_chip(s: &DaemonState, connected: bool) -> (egui::Color32, String) {
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
