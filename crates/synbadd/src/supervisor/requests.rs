//! IPC request dispatcher.
//!
//! All client-initiated calls funnel through `handle_request`. Each arm is
//! a small, mostly self-contained translation from a `Request` variant to
//! a `Response` — anything non-trivial (config edits, core lifecycle) is
//! a thin call into a sibling module, so this stays a routing table.

use synbad_ipc::server::IncomingRequest;
use synbad_ipc::{Event, Request, Response};

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
                        let _ = self.events.send(Event::TrustRevoked {
                            machine_id: machine_id.clone(),
                        });
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
        };
        let _ = reply.send(response);
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
