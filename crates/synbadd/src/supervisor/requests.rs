//! IPC request dispatcher.
//!
//! All client-initiated calls funnel through `handle_request`. Each arm is
//! a small, mostly self-contained translation from a `Request` variant to
//! a `Response` — anything non-trivial (config edits, core lifecycle) is
//! a thin call into a sibling module, so this stays a routing table.

use std::time::Duration;
use synbad_audio::{bridge::DeviceListReply, peer_audio_active, AudioBridge, AudioCommand};
use synbad_config::paths;
use synbad_ipc::server::IncomingRequest;
use synbad_ipc::{Event, Request, Response};
use tokio::sync::oneshot;

use crate::pairing;

use super::{Supervisor, MIN_BACKOFF};

impl Supervisor {
    pub(super) async fn handle_request(&mut self, req: IncomingRequest) {
        let IncomingRequest { request, reply, .. } = req;
        let response = match request {
            Request::GetStatus => Response::Status {
                state: self.state.clone(),
                recent_log: self.log_tail.iter().cloned().collect(),
                connected_peers: self.connected_peers.iter().cloned().collect(),
                active_screen: self.active_screen.clone(),
            },
            Request::GetConfig => Response::Config {
                config: Box::new(self.config.clone()),
            },
            Request::SetConfig { config } => match self.set_config(*config).await {
                Ok(()) => Response::Ok,
                Err(e) => Response::Error {
                    message: e.to_string(),
                },
            },
            Request::Start => {
                self.desired_running = true;
                persist_user_stopped(false);
                // Explicit Start resets retry backoff for an immediate attempt.
                self.fast_fail_count = 0;
                self.backoff = MIN_BACKOFF;
                self.start_core().await;
                // Audio rides with input: bring the bridge up alongside
                // the Core so a started Synbad always asserts its (enabled)
                // audio session. Best-effort — a failed bring-up surfaces
                // its own AudioError and never blocks the Core starting.
                let _ = self.reconcile_audio_subsystem().await;
                Response::Ok
            }
            Request::Stop => {
                self.desired_running = false;
                // Only an explicit Stop keeps sharing off across a daemon
                // restart; every other launch starts it automatically.
                persist_user_stopped(true);
                self.fast_fail_count = 0;
                self.stop_core().await;
                // Input and audio deactivate together: tear the bridge
                // down so a stopped Synbad has no lingering audio session.
                let _ = self.reconcile_audio_subsystem().await;
                Response::Ok
            }
            Request::Restart => {
                self.desired_running = true;
                persist_user_stopped(false);
                self.restart_core().await;
                // `desired_running` stays true across a restart, so this
                // just re-asserts audio (no-op if the session survived).
                let _ = self.reconcile_audio_subsystem().await;
                Response::Ok
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
                            let _ = handle.commands_tx.try_send(AudioCommand::ClosePeer {
                                peer_machine_id: machine_id.clone(),
                            });
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
                    .try_send(AudioCommand::ListDevices { reply: tx })
                    .is_err()
                {
                    return Response::Error {
                        message: "audio bridge is not responding".into(),
                    };
                }
                tokio::time::timeout(Duration::from_secs(2), rx)
                    .await
                    .map_err(|_| "audio bridge response timed out".to_string())
                    .and_then(|reply| {
                        reply.map_err(|_| "audio bridge dropped reply channel".to_string())
                    })
            }
            None => tokio::time::timeout(
                Duration::from_secs(2),
                tokio::task::spawn_blocking(AudioBridge::list_devices_blocking),
            )
            .await
            .map_err(|_| "audio device enumeration timed out".to_string())
            .and_then(|reply| reply.map_err(|e| e.to_string()))
            .and_then(|reply| reply.map_err(|e| e.to_string())),
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
    /// `audio.enabled` is now hot-reloadable: the supervisor brings the
    /// bridge + listener up (or tears them down) live via
    /// [`Supervisor::ensure_audio_subsystem`] /
    /// [`Supervisor::teardown_audio_subsystem`]. If the live bring-up
    /// fails (e.g. the signal port is in use) we surface the old
    /// "restart required" error so the user has actionable feedback.
    async fn update_audio_config(
        &mut self,
        audio: synbad_config::AudioConfig,
    ) -> anyhow::Result<()> {
        let old_audio = self.config.audio.clone();
        let mut new_config = self.config.clone();
        new_config.audio = audio.clone();

        // Persist the new config first so `ensure_audio_subsystem` /
        // teardown see the up-to-date master switch.
        self.set_config(new_config).await?;

        // Audio activation is coupled to the run state: enabling the
        // toggle only brings the bridge up if Synbad is started. Route
        // through the same level-triggered reconcile the Start/Stop
        // handlers use so the toggle and the run state never diverge.
        // Errors are surfaced to the GUI inside the reconcile.
        if old_audio.enabled != audio.enabled {
            let _ = self.reconcile_audio_subsystem().await;
        }
        // Push the live bridge a Reconfigure so device picks / per-peer
        // toggles take effect immediately.
        if let Some(handle) = &self.audio {
            let _ = handle
                .commands_tx
                .try_send(AudioCommand::Reconfigure(audio.clone()));
        }
        // Pick up "newly enabled" peers (master toggle was already on
        // and a per_peer entry just turned on, or globals turned on).
        // The reconcile loop is the single dial entry-point; it'll skip
        // peers that became inactive and pick up the newly-active ones.
        if old_audio.enabled && audio.enabled {
            let demoted: Vec<String> = self
                .peers
                .keys()
                .filter(|id| peer_audio_active(&old_audio, id) && !peer_audio_active(&audio, id))
                .cloned()
                .collect();
            // Demoted peers must be evicted from `audio_live` so the
            // reconcile loop doesn't think they're still up if the user
            // later re-enables them.
            for id in &demoted {
                self.audio_live.remove(id);
                self.audio_backoff.remove(id);
            }
        }
        self.reconcile_audio_sessions();
        Ok(())
    }

    /// Snapshot per-peer audio status from the bridge.
    async fn audio_status_snapshot(&self) -> Response {
        let Some(handle) = &self.audio else {
            return Response::AudioStatus { peers: Vec::new() };
        };
        let (tx, rx) = oneshot::channel();
        if handle
            .commands_tx
            .try_send(AudioCommand::QueryStatus { reply: tx })
            .is_err()
        {
            return Response::Error {
                message: "audio bridge is not responding".into(),
            };
        }
        match tokio::time::timeout(Duration::from_secs(2), rx).await {
            Ok(Ok(peers)) => Response::AudioStatus { peers },
            _ => Response::Error {
                message: "audio bridge did not reply within two seconds".into(),
            },
        }
    }

    fn start_pairing(&mut self, machine_id: &str) -> anyhow::Result<()> {
        self.gc_pairing_tasks();
        anyhow::ensure!(
            self.pairing_tasks.len() < 64,
            "too many pairing sessions; retry shortly"
        );
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

/// Record (or clear) the user's explicit Stop so the next daemon launch
/// honours it. Best-effort: failing to persist only means the next launch
/// falls back to the default of starting.
fn persist_user_stopped(stopped: bool) {
    let marker = paths::user_stopped_marker();
    let res = if stopped {
        if let Some(parent) = marker.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        std::fs::write(&marker, b"")
    } else {
        match std::fs::remove_file(&marker) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            other => other,
        }
    };
    if let Err(e) = res {
        tracing::warn!(?e, ?marker, "could not persist stop state");
    }
}
