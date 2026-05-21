//! Top-level orchestration: owns one [`AudioSession`] per peer and
//! surfaces commands/events through tokio mpsc channels so the `synbadd`
//! supervisor can drive it from its central `select!` loop.

use std::collections::HashMap;
use std::sync::Arc;

use synbad_config::AudioConfig;
use synbad_crypto::CipherStream;
use synbad_discovery::{Identity, TrustedPeerStore};
use synbad_ipc::{AudioDeviceInfo, PeerAudioStatus};
use tokio::sync::{mpsc, Mutex};
use tokio::task::JoinHandle;
use tracing::{info, warn};

use crate::devices;
use crate::errors::AudioError;
use crate::session::{AudioSession, SessionRole};

/// Commands the supervisor sends into the bridge. Not `Debug` because
/// some variants carry a `CipherStream` (which can't be cheaply printed
/// and shouldn't accidentally leak handshake material into logs).
pub enum AudioCommand {
    /// Hand off a freshly-handshaken authenticated signaling stream from
    /// a remote peer. The bridge owns it from here on; `role` is set
    /// from the TCP direction (listener accept → answerer, outbound
    /// dial → offerer).
    IncomingSignal {
        peer_machine_id: String,
        stream: CipherStream,
        role: SessionRole,
    },
    /// Apply a new configuration. Bridge will diff and reconfigure
    /// individual sessions as needed.
    Reconfigure(AudioConfig),
    /// Probe cpal and reply with the current device list. One-shot reply.
    ListDevices {
        reply: tokio::sync::oneshot::Sender<DeviceListReply>,
    },
    /// Snapshot all peer statuses. One-shot reply.
    QueryStatus {
        reply: tokio::sync::oneshot::Sender<Vec<PeerAudioStatus>>,
    },
    /// Tear down an active audio session for a specific peer. Used when
    /// a peer's trust is revoked or when a hot config reload disables
    /// the peer's routing.
    ClosePeer { peer_machine_id: String },
    /// Graceful shutdown: close all sessions and exit the run loop.
    Shutdown,
}

#[derive(Debug)]
pub struct DeviceListReply {
    pub input: Vec<AudioDeviceInfo>,
    pub output: Vec<AudioDeviceInfo>,
}

/// Events emitted out of the bridge into the supervisor's event bus.
#[derive(Debug, Clone)]
pub enum AudioEvent {
    PeerStatus(PeerAudioStatus),
    Error {
        peer: Option<String>,
        message: String,
    },
    /// OS notified us that the set of devices changed. The supervisor
    /// forwards this to the GUI so it can refresh dropdowns.
    DevicesChanged,
    /// Bridge removed a session from its map (glare replace, reconfigure,
    /// trust revoke, shutdown drain, or a session reporting fatal
    /// transport state). Supervisor uses this to evict its liveness cache
    /// so the reconcile loop will re-dial without waiting for the next
    /// mDNS refresh.
    SessionClosed { peer: String },
}

/// Handle the supervisor keeps after spawning the bridge.
pub struct AudioBridgeHandle {
    pub commands_tx: mpsc::Sender<AudioCommand>,
    pub events_rx: mpsc::Receiver<AudioEvent>,
}

/// The bridge itself. Built off-task, then [`Self::spawn`] consumes it and
/// returns the handle + the join handle for the run loop.
pub struct AudioBridge {
    config: AudioConfig,
    _identity: Arc<Identity>,
    _trust: Arc<Mutex<TrustedPeerStore>>,
    sessions: HashMap<String, AudioSession>,
}

impl AudioBridge {
    pub fn new(
        config: AudioConfig,
        identity: Arc<Identity>,
        trust: Arc<Mutex<TrustedPeerStore>>,
    ) -> Self {
        Self {
            config,
            _identity: identity,
            _trust: trust,
            sessions: HashMap::new(),
        }
    }

    pub fn spawn(self) -> (AudioBridgeHandle, JoinHandle<()>) {
        let (commands_tx, commands_rx) = mpsc::channel::<AudioCommand>(64);
        let (events_tx, events_rx) = mpsc::channel::<AudioEvent>(256);
        let task = tokio::spawn(self.run(commands_rx, events_tx));
        (
            AudioBridgeHandle {
                commands_tx,
                events_rx,
            },
            task,
        )
    }

    async fn run(
        mut self,
        mut commands: mpsc::Receiver<AudioCommand>,
        events: mpsc::Sender<AudioEvent>,
    ) {
        info!(
            enabled = self.config.enabled,
            "audio bridge run loop starting"
        );
        while let Some(cmd) = commands.recv().await {
            match cmd {
                AudioCommand::IncomingSignal {
                    peer_machine_id,
                    stream,
                    role,
                } => {
                    if !self.config.enabled {
                        // Politely drop the stream by letting it go out of scope.
                        warn!(peer = %peer_machine_id, "incoming audio signal while disabled");
                        continue;
                    }
                    // Glare resolution: if a session already exists for
                    // this peer, the newer connection wins on the
                    // assumption that the older one is stale. Closing
                    // the old session aborts its tasks via Drop.
                    if let Some(old) = self.sessions.remove(&peer_machine_id) {
                        warn!(peer = %peer_machine_id, "replacing existing audio session");
                        old.close(Some("superseded by new connection".into())).await;
                        let _ = events
                            .send(AudioEvent::SessionClosed {
                                peer: peer_machine_id.clone(),
                            })
                            .await;
                    }
                    match AudioSession::start(
                        peer_machine_id.clone(),
                        stream,
                        &self.config,
                        role,
                        events.clone(),
                    )
                    .await
                    {
                        Ok(session) => {
                            self.sessions.insert(peer_machine_id, session);
                        }
                        Err(e) => {
                            let _ = events
                                .send(AudioEvent::Error {
                                    peer: Some(peer_machine_id),
                                    message: e.to_string(),
                                })
                                .await;
                        }
                    }
                }
                AudioCommand::Reconfigure(new_cfg) => {
                    let old_cfg = std::mem::replace(&mut self.config, new_cfg);
                    let new_cfg = &self.config;
                    // A device swap invalidates every cpal stream we hold,
                    // so every session needs a clean restart. Per-peer
                    // routing flipping off is the other "active state
                    // changed" case — a peer that was flowing audio and is
                    // now disabled should stop immediately rather than
                    // wait for natural reconnect.
                    let device_changed = old_cfg.input_device != new_cfg.input_device
                        || old_cfg.output_device != new_cfg.output_device;
                    let peers_to_close: Vec<String> = self
                        .sessions
                        .keys()
                        .filter(|peer| {
                            device_changed
                                || (peer_audio_active(&old_cfg, peer)
                                    && !peer_audio_active(new_cfg, peer))
                        })
                        .cloned()
                        .collect();
                    for peer in peers_to_close {
                        if let Some(session) = self.sessions.remove(&peer) {
                            info!(peer = %peer, "closing session on reconfigure");
                            session.close(Some("reconfigure".into())).await;
                            let _ = events
                                .send(AudioEvent::SessionClosed { peer: peer.clone() })
                                .await;
                        }
                    }
                    // Flip-on doesn't auto-dial: the supervisor walks
                    // visible peers and calls `maybe_dial_audio` for any
                    // newly-enabled ones (see `update_audio_config`).
                }
                AudioCommand::ListDevices { reply } => {
                    let input = devices::list_input_devices().unwrap_or_default();
                    let output = devices::list_output_devices().unwrap_or_default();
                    let _ = reply.send(DeviceListReply { input, output });
                }
                AudioCommand::QueryStatus { reply } => {
                    let statuses: Vec<PeerAudioStatus> =
                        self.sessions.values().map(|s| s.status()).collect();
                    let _ = reply.send(statuses);
                }
                AudioCommand::ClosePeer { peer_machine_id } => {
                    if let Some(session) = self.sessions.remove(&peer_machine_id) {
                        info!(peer = %peer_machine_id, "closing session on request");
                        session.close(Some("closed by supervisor".into())).await;
                        let _ = events
                            .send(AudioEvent::SessionClosed {
                                peer: peer_machine_id,
                            })
                            .await;
                    }
                }
                AudioCommand::Shutdown => {
                    info!("audio bridge shutdown requested");
                    break;
                }
            }
        }

        // Drain sessions on exit.
        for (peer, session) in self.sessions.drain() {
            session.close(Some("bridge shutting down".into())).await;
            let _ = events.send(AudioEvent::SessionClosed { peer }).await;
        }
        info!("audio bridge run loop exited");
    }

    /// Synchronous device listing for IPC contexts where we don't have an
    /// async response channel handy.
    pub fn list_devices_blocking() -> Result<DeviceListReply, AudioError> {
        Ok(DeviceListReply {
            input: devices::list_input_devices()?,
            output: devices::list_output_devices()?,
        })
    }
}

/// Whether this peer should have an active audio session under the given
/// config. A peer is "active" iff the master toggle is on and either
/// send-to-peer or receive-from-peer would do real work. The supervisor
/// and the bridge both consult this — the bridge to decide what to close
/// on reconfigure, the supervisor to decide what to dial.
pub fn peer_audio_active(cfg: &AudioConfig, peer: &str) -> bool {
    if !cfg.enabled {
        return false;
    }
    match cfg.per_peer.get(peer) {
        Some(routing) => routing.enabled && (routing.send_to_peer || routing.receive_from_peer),
        None => cfg.send_mic_to_peers || cfg.receive_peer_audio,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use synbad_config::PeerAudioRouting;

    fn cfg_default() -> AudioConfig {
        AudioConfig::default()
    }

    #[test]
    fn peer_audio_active_off_when_master_disabled() {
        let mut cfg = cfg_default();
        cfg.enabled = false;
        cfg.send_mic_to_peers = true;
        assert!(!peer_audio_active(&cfg, "peer-X"));
    }

    #[test]
    fn peer_audio_active_uses_globals_without_override() {
        let mut cfg = cfg_default();
        cfg.enabled = true;
        cfg.send_mic_to_peers = true;
        assert!(peer_audio_active(&cfg, "peer-X"));

        cfg.send_mic_to_peers = false;
        cfg.receive_peer_audio = false;
        assert!(!peer_audio_active(&cfg, "peer-X"));
    }

    #[test]
    fn peer_audio_active_respects_per_peer_override() {
        let mut cfg = cfg_default();
        cfg.enabled = true;
        // Globals would say off, but per_peer flips on.
        cfg.per_peer.insert(
            "peer-X".into(),
            PeerAudioRouting {
                enabled: true,
                send_to_peer: true,
                receive_from_peer: false,
            },
        );
        assert!(peer_audio_active(&cfg, "peer-X"));

        // Disabled override beats global-on.
        cfg.send_mic_to_peers = true;
        cfg.per_peer.get_mut("peer-X").unwrap().enabled = false;
        assert!(!peer_audio_active(&cfg, "peer-X"));
    }
}
