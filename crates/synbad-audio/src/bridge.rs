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
use crate::session::AudioSession;

/// Commands the supervisor sends into the bridge. Not `Debug` because
/// some variants carry a `CipherStream` (which can't be cheaply printed
/// and shouldn't accidentally leak handshake material into logs).
pub enum AudioCommand {
    /// Hand off a freshly-accepted authenticated signaling stream from a
    /// remote peer. The bridge owns it from here on.
    IncomingSignal {
        peer_machine_id: String,
        stream: CipherStream,
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
                } => {
                    if !self.config.enabled {
                        // Politely drop the stream by letting it go out of scope.
                        warn!(peer = %peer_machine_id, "incoming audio signal while disabled");
                        continue;
                    }
                    match AudioSession::new(peer_machine_id.clone(), stream).await {
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
                    self.config = new_cfg;
                    // V1: no diffing yet; future commits will tear down and
                    // rebuild sessions affected by the change.
                }
                AudioCommand::ListDevices { reply } => {
                    let input = devices::list_input_devices().unwrap_or_default();
                    let output = devices::list_output_devices().unwrap_or_default();
                    let _ = reply.send(DeviceListReply { input, output });
                }
                AudioCommand::QueryStatus { reply } => {
                    let statuses: Vec<PeerAudioStatus> = self
                        .sessions
                        .keys()
                        .map(|peer| PeerAudioStatus {
                            machine_id: peer.clone(),
                            display_name: peer.clone(),
                            sending_to_peer: false,
                            receiving_from_peer: false,
                            rtt_ms: None,
                            last_error: None,
                        })
                        .collect();
                    let _ = reply.send(statuses);
                }
                AudioCommand::Shutdown => {
                    info!("audio bridge shutdown requested");
                    break;
                }
            }
        }

        // Drain sessions on exit.
        for (_peer, session) in self.sessions.drain() {
            session.close(Some("bridge shutting down".into())).await;
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
