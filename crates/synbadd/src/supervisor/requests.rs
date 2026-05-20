//! IPC request dispatcher.
//!
//! All client-initiated calls funnel through `handle_request`. Each arm is
//! a small, mostly self-contained translation from a `Request` variant to
//! a `Response` — anything non-trivial (config edits, core lifecycle) is
//! a thin call into a sibling module, so this stays a routing table.

use synbad_audio::{bridge::DeviceListReply, peer_audio_active, AudioBridge, AudioCommand};
use synbad_ipc::server::IncomingRequest;
use synbad_ipc::{Event, Request, Response};
use tokio::sync::oneshot;

use crate::pairing;

use super::Supervisor;

impl Supervisor {
    pub(super) async fn handle_request(&mut self, req: IncomingRequest) {
        let IncomingRequest { request, reply, .. } = req;
        let response = match request {
            Request::GetStatus => Response::Status {
                state: self.state.clone(),
                recent_log: self.log_tail.iter().cloned().collect(),
            },
            Request::GetConfig => Response::Config {
                config: self.config.clone(),
            },
            Request::SetConfig { config } => match self.set_config(config).await {
                Ok(()) => Response::Ok,
                Err(e) => Response::Error {
                    message: e.to_string(),
                },
            },
            Request::Start => {
                self.desired_running = true;
                // Explicit user action resets the give-up state from a
                // prior instant-fail loop — they may have fixed the
                // missing-deps issue and want us to try again.
                self.fast_fail_count = 0;
                match self.start_core().await {
                    Ok(()) => Response::Ok,
                    Err(e) => Response::Error {
                        message: e.to_string(),
                    },
                }
            }
            Request::Stop => {
                self.desired_running = false;
                self.fast_fail_count = 0;
                self.stop_core().await;
                Response::Ok
            }
            Request::Restart => {
                self.desired_running = true;
                self.fast_fail_count = 0;
                self.stop_core().await;
                match self.start_core().await {
                    Ok(()) => Response::Ok,
                    Err(e) => Response::Error {
                        message: e.to_string(),
                    },
                }
            }
            Request::Subscribe => Response::Ok,
            Request::ListPeers => Response::Peers {
                peers: self.peers.values().cloned().collect(),
            },
            Request::GetLocalIdentity => Response::LocalIdentity {
                machine_id: self.identity.machine_id.to_string(),
                fingerprint: self.identity.fingerprint.clone(),
            },
            Request::StartPairing { machine_id } => match self.start_pairing(&machine_id) {
                Ok(()) => Response::Ok,
                Err(e) => Response::Error {
                    message: e.to_string(),
                },
            },
            Request::ConfirmPairing { session_id, accept } => {
                match self.pairing_confirm.remove(&session_id) {
                    Some(tx) => {
                        let _ = tx.send(accept);
                        Response::Ok
                    }
                    None => Response::Error {
                        message: format!("no pending pairing session {:?}", session_id),
                    },
                }
            }
            Request::ListTrustedPeers => {
                let trust = self.trust.lock().await;
                Response::TrustedPeers {
                    peers: trust.list().to_vec(),
                }
            }
            Request::Shutdown => {
                // Flip the flag; the run loop tears down after this
                // response is flushed (see `Supervisor::run`).
                self.shutdown = true;
                Response::Ok
            }
            Request::RevokeTrust { machine_id } => {
                let mut trust = self.trust.lock().await;
                match trust.remove(&machine_id) {
                    Ok(true) => {
                        drop(trust);
                        let _ = self.events.send(Event::TrustRevoked {
                            machine_id: machine_id.clone(),
                        });
                        // Tear down any active audio session — a revoked
                        // peer must not keep streaming. Best-effort: if
                        // the bridge is disabled or has died we skip it.
                        if let Some(handle) = &self.audio {
                            let _ = handle
                                .commands_tx
                                .send(AudioCommand::ClosePeer {
                                    peer_machine_id: machine_id.clone(),
                                })
                                .await;
                        }
                        Response::Ok
                    }
                    Ok(false) => Response::Error {
                        message: format!("peer {:?} is not trusted", machine_id),
                    },
                    Err(e) => Response::Error {
                        message: e.to_string(),
                    },
                }
            }
            Request::ListAudioDevices => self.list_audio_devices().await,
            Request::SetAudioConfig { config } => match self.update_audio_config(config).await {
                Ok(()) => Response::Ok,
                Err(e) => Response::Error {
                    message: e.to_string(),
                },
            },
            Request::GetAudioStatus => self.audio_status_snapshot().await,
        };
        let _ = reply.send(response);
    }

    /// Enumerate audio devices. Works whether or not the audio bridge is
    /// running: when the bridge is enabled we ask it (so a single source
    /// of truth handles cpal threading), otherwise we probe cpal directly
    /// so the GUI can populate dropdowns before the user opts in.
    async fn list_audio_devices(&self) -> Response {
        let reply: Result<DeviceListReply, String> = match &self.audio {
            Some(handle) => {
                let (tx, rx) = oneshot::channel();
                if handle
                    .commands_tx
                    .send(AudioCommand::ListDevices { reply: tx })
                    .await
                    .is_err()
                {
                    return Response::Error {
                        message: "audio bridge is not responding".into(),
                    };
                }
                rx.await
                    .map_err(|_| "audio bridge dropped reply channel".to_string())
            }
            None => AudioBridge::list_devices_blocking().map_err(|e| e.to_string()),
        };
        match reply {
            Ok(list) => Response::AudioDevices {
                input: list.input,
                output: list.output,
            },
            Err(e) => Response::Error { message: e },
        }
    }

    /// Update the audio sub-section of the config. Toggling
    /// `audio.enabled` live is *not* supported in v1 — the listener and
    /// bridge are only built at startup. We log a warning when the flag
    /// flips so the user knows a restart is needed.
    async fn update_audio_config(
        &mut self,
        audio: synbad_config::AudioConfig,
    ) -> anyhow::Result<()> {
        let old_audio = self.config.audio.clone();
        let mut new_config = self.config.clone();
        new_config.audio = audio.clone();
        if old_audio.enabled != audio.enabled {
            tracing::warn!(
                from = old_audio.enabled,
                to = audio.enabled,
                "audio.enabled toggled — a daemon restart is required for the change to take effect"
            );
        }
        // Push the live bridge a Reconfigure so device picks / per-peer
        // toggles take effect immediately. enabled changes still need a
        // restart, but everything else is hot-reloadable.
        if let Some(handle) = &self.audio {
            let _ = handle
                .commands_tx
                .send(AudioCommand::Reconfigure(audio.clone()))
                .await;
        }
        // Pick up "newly enabled" peers (master toggle was already on and
        // a per_peer entry just turned on, or globals turned on). Bridge
        // doesn't dial outbound itself; the supervisor owns
        // `maybe_dial_audio`. Glare rule dedupes any redundant dial.
        if old_audio.enabled && audio.enabled {
            let newly_enabled: Vec<synbad_ipc::DiscoveredPeer> = self
                .peers
                .values()
                .filter(|p| {
                    !peer_audio_active(&old_audio, &p.machine_id)
                        && peer_audio_active(&audio, &p.machine_id)
                })
                .cloned()
                .collect();
            for peer in newly_enabled {
                self.maybe_dial_audio(peer);
            }
        }
        self.set_config(new_config).await
    }

    /// Snapshot per-peer audio status from the bridge.
    async fn audio_status_snapshot(&self) -> Response {
        let Some(handle) = &self.audio else {
            return Response::AudioStatus { peers: Vec::new() };
        };
        let (tx, rx) = oneshot::channel();
        if handle
            .commands_tx
            .send(AudioCommand::QueryStatus { reply: tx })
            .await
            .is_err()
        {
            return Response::Error {
                message: "audio bridge is not responding".into(),
            };
        }
        match rx.await {
            Ok(peers) => Response::AudioStatus { peers },
            Err(_) => Response::Error {
                message: "audio bridge dropped reply channel".into(),
            },
        }
    }

    fn start_pairing(&mut self, machine_id: &str) -> anyhow::Result<()> {
        let peer = self
            .peers
            .get(machine_id)
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("peer {:?} not currently discovered", machine_id))?;
        let handle = pairing::spawn_outbound(peer, self.pairing_deps.clone());
        self.pairing_confirm
            .insert(handle.session_id.clone(), handle.confirm_tx);
        self.pairing_tasks.push(handle._task);
        self.gc_pairing_tasks();
        Ok(())
    }
}
