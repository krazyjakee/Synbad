//! Daemon-side pairing: TCP listener, outbound dialer, per-session state
//! machine.
//!
//! The wire-level transcript / signature / SAS logic lives in
//! `synbad_discovery::pairing`. This module just plumbs it onto TCP and
//! bridges to the supervisor via channels.
//!
//! ### Session lifecycle
//!
//! ```text
//!   open TCP    send Hello       recv Hello        wait user      send Confirm    recv Confirm    persist
//!   ────────► ─────────────► ───────────────► ─────────────► ───────────────► ─────────────► ────────
//! ```
//!
//! Either side can be initiator; the protocol is symmetric. After
//! exchanging Hellos, both sides compute the same SAS and emit
//! `Event::PairingProposed` to the supervisor (which forwards it onto the
//! IPC bus). The user clicks Accept/Decline in the GUI; that flows back as
//! `Request::ConfirmPairing` and the supervisor routes it to the right
//! session via its oneshot map. Each side then sends its signed
//! `PairConfirm` and verifies the peer's. If both accepted and both
//! signatures verify, trust is persisted.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use rand_core::RngCore;
use serde::{Deserialize, Serialize};
use tokio::net::TcpStream;
use tokio::sync::{broadcast, oneshot};

use synbad_crypto::{
    accept as crypto_accept, initiate as crypto_initiate, CipherStream, HandshakeMode,
};
use synbad_discovery::pairing::{
    canonical_transcript, sas_code, sign_transcript, verify_signature, PairConfirm, PairHello,
};
use synbad_discovery::{Identity, TrustedPeer, TrustedPeerStore};
use synbad_ipc::{DiscoveredPeer, Event};

/// Wall-clock budget for an entire pairing session, from connect to
/// persist. If the user takes longer to confirm than this, the session
/// fails and the sockets close — re-pair to retry.
const SESSION_TIMEOUT: Duration = Duration::from_secs(120);

/// JSON-lines envelope on the pairing TCP connection.
#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum PairMessage {
    Hello(PairHello),
    Confirm(PairConfirm),
}

/// Spawn the TCP listener and return its handle. The listener accepts
/// inbound pairing sessions until the handle is aborted.
pub async fn spawn_listener(
    bind_port: u16,
    deps: Arc<SessionDeps>,
    incoming_tx: tokio::sync::mpsc::Sender<IncomingSession>,
) -> Result<tokio::task::JoinHandle<()>> {
    let listener = crate::transport::bind_listener(bind_port)
        .await
        .with_context(|| format!("binding pairing listener on :{}", bind_port))?;
    tracing::info!(port = bind_port, "pairing listener up");

    let handle = tokio::spawn(async move {
        let slots = Arc::new(tokio::sync::Semaphore::new(64));
        loop {
            let Ok(slot) = slots.clone().acquire_owned().await else {
                break;
            };
            match listener.accept().await {
                Ok((stream, addr)) => {
                    let (confirm_tx, confirm_rx) = oneshot::channel::<bool>();
                    let session_id = new_session_id();
                    let deps = deps.clone();
                    let session_id_for_task = session_id.clone();
                    let (registered_tx, registered_rx) = oneshot::channel();
                    let task = tokio::spawn(async move {
                        let _slot = slot;
                        if registered_rx.await.is_err() {
                            return;
                        }
                        let _ =
                            run_session(session_id_for_task, stream, addr, deps, confirm_rx, None)
                                .await;
                    });
                    if let Err(e) = incoming_tx
                        .send(IncomingSession {
                            session_id,
                            confirm_tx,
                            registered: registered_tx,
                            _task: task,
                        })
                        .await
                    {
                        e.0._task.abort();
                        break;
                    }
                }
                Err(e) => {
                    tracing::warn!(?e, "pairing accept failed");
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }
            }
        }
    });
    Ok(handle)
}

/// Initiate a pairing session against a discovered peer. Returns the
/// session_id immediately; the actual handshake runs in a spawned task and
/// reports progress via the event bus carried in [`SessionDeps`].
pub fn spawn_outbound(peer: DiscoveredPeer, deps: Arc<SessionDeps>) -> OutboundHandle {
    let (confirm_tx, confirm_rx) = oneshot::channel::<bool>();
    let session_id = new_session_id();
    let session_id_for_task = session_id.clone();
    let task = tokio::spawn(async move {
        let addr_str = format!("{}:{}", peer.host, peer.service_port);
        let stream =
            match crate::transport::connect_peer(&peer, peer.service_port, Duration::from_secs(10))
                .await
            {
                Ok(stream) => stream,
                Err(e) => {
                    let _ = deps.events.send(Event::PairingFailed {
                        session_id: session_id_for_task,
                        reason: format!("connect to {}: {}", addr_str, e),
                    });
                    return;
                }
            };
        let _ = run_session(
            session_id_for_task,
            stream,
            stream_peer_addr(&peer),
            deps,
            confirm_rx,
            Some(peer.machine_id.clone()),
        )
        .await;
    });
    OutboundHandle {
        session_id,
        confirm_tx,
        _task: task,
    }
}

fn stream_peer_addr(peer: &DiscoveredPeer) -> SocketAddr {
    // We don't actually need the real peer SocketAddr for the protocol —
    // only for diagnostics. Fall back to a zero placeholder if parsing
    // fails (the host could be a hostname.local).
    peer.host
        .parse::<std::net::IpAddr>()
        .map(|ip| SocketAddr::new(ip, peer.service_port))
        .unwrap_or_else(|_| SocketAddr::from(([0, 0, 0, 0], peer.service_port)))
}

/// Things every session task needs. Wrapped in `Arc` so the listener can
/// hand the same set out to many tasks without cloning the inner data.
pub struct SessionDeps {
    pub identity: Arc<Identity>,
    pub trust: Arc<tokio::sync::Mutex<TrustedPeerStore>>,
    pub events: broadcast::Sender<Event>,
    /// Name we announce to peers during pairing. Snapshot of the
    /// config's `server_name` at daemon startup; if the user renames the
    /// machine, restart the daemon to update.
    pub display_name: String,
}

/// Returned by the listener when a new inbound session opens. The
/// supervisor adds `confirm_tx` to its map so a later
/// `Request::ConfirmPairing` can be routed to the right session.
pub struct IncomingSession {
    pub session_id: String,
    pub confirm_tx: oneshot::Sender<bool>,
    pub registered: oneshot::Sender<()>,
    /// The supervisor owns this task and aborts it on shutdown.
    pub _task: tokio::task::JoinHandle<()>,
}

pub struct OutboundHandle {
    pub session_id: String,
    pub confirm_tx: oneshot::Sender<bool>,
    pub _task: tokio::task::JoinHandle<()>,
}

async fn run_session(
    session_id: String,
    stream: TcpStream,
    peer_addr: SocketAddr,
    deps: Arc<SessionDeps>,
    user_confirm_rx: oneshot::Receiver<bool>,
    expected_peer_id: Option<String>,
) -> Result<()> {
    let result = tokio::time::timeout(
        SESSION_TIMEOUT,
        run_session_inner(
            &session_id,
            stream,
            peer_addr,
            deps.clone(),
            user_confirm_rx,
            expected_peer_id,
        ),
    )
    .await;

    match result {
        Ok(Ok(())) => Ok(()),
        Ok(Err(e)) => {
            tracing::warn!(?session_id, ?e, "pairing session failed");
            let _ = deps.events.send(Event::PairingFailed {
                session_id,
                reason: e.to_string(),
            });
            Err(e)
        }
        Err(_) => {
            let reason = "pairing session timed out".to_string();
            tracing::warn!(?session_id, "{}", reason);
            let _ = deps.events.send(Event::PairingFailed {
                session_id,
                reason: reason.clone(),
            });
            bail!(reason)
        }
    }
}

async fn run_session_inner(
    session_id: &str,
    stream: TcpStream,
    peer_addr: SocketAddr,
    deps: Arc<SessionDeps>,
    user_confirm_rx: oneshot::Receiver<bool>,
    expected_peer_id: Option<String>,
) -> Result<()> {
    // ── Step 0: encrypted transport (anonymous — pairing bootstraps trust)
    //
    // Pairing can't yet authenticate the peer's long-term key (that's
    // *what* it's doing), so the transport handshake is anonymous: it
    // gives us confidentiality + integrity against passive listeners
    // and ties the SAS to the channel via [`transcript_hash`]. An
    // active MITM is still detected by the user's SAS comparison.
    let is_initiator = expected_peer_id.is_some();
    let mut chan: CipherStream = if is_initiator {
        let (chan, _) = tokio::time::timeout(
            Duration::from_secs(10),
            crypto_initiate(stream, HandshakeMode::Anonymous, None),
        )
        .await
        .map_err(|_| anyhow!("pairing transport handshake timed out"))?
        .with_context(|| format!("anonymous transport handshake to {}", peer_addr))?;
        chan
    } else {
        let (chan, _) = tokio::time::timeout(
            Duration::from_secs(10),
            crypto_accept(stream, HandshakeMode::Anonymous, |_| None),
        )
        .await
        .map_err(|_| anyhow!("pairing transport handshake timed out"))?
        .with_context(|| format!("anonymous transport handshake from {}", peer_addr))?;
        chan
    };

    // ── Step 1: exchange Hellos ──────────────────────────────────────
    let mut nonce = [0u8; 16];
    rand_core::OsRng.fill_bytes(&mut nonce);
    let our_hello = PairHello {
        pubkey_hex: hex::encode(deps.identity.public_key),
        nonce_hex: hex::encode(nonce),
        machine_id: deps.identity.machine_id.to_string(),
        display_name: deps.display_name.clone(),
    };
    send_msg(&mut chan, &PairMessage::Hello(our_hello.clone())).await?;

    let peer_hello = match tokio::time::timeout(Duration::from_secs(10), recv_msg(&mut chan))
        .await
        .map_err(|_| anyhow!("pairing Hello timed out"))??
    {
        PairMessage::Hello(h) => h,
        other => bail!("expected Hello, got {:?}", other),
    };

    // Sanity: if we initiated against a specific peer machine_id, refuse
    // to keep going if the responder identifies differently.
    if let Some(expected) = &expected_peer_id {
        if expected != &peer_hello.machine_id {
            bail!(
                "peer at {} identifies as {} but we initiated against {}",
                peer_addr,
                peer_hello.machine_id,
                expected
            );
        }
    }
    if peer_hello.machine_id == deps.identity.machine_id.to_string() {
        bail!("rejected self-pairing attempt");
    }

    // ── Step 2: derive transcript + SAS, propose to user ────────────
    let transcript = canonical_transcript(&our_hello, &peer_hello);
    let sas = sas_code(&transcript);
    tracing::info!(?session_id, peer = %peer_hello.machine_id, %sas, "pairing handshake done; awaiting user confirm");

    let _ = deps.events.send(Event::PairingProposed {
        session_id: session_id.to_string(),
        peer_machine_id: peer_hello.machine_id.clone(),
        peer_display_name: peer_hello.display_name.clone(),
        peer_fingerprint: synbad_discovery::identity::fingerprint_for(&pubkey_from_hex(
            &peer_hello.pubkey_hex,
        )?),
        verification_code: sas.clone(),
    });

    // ── Step 3: wait for our user, send our PairConfirm ─────────────
    // We send our Confirm before reading the peer's — the protocol is
    // symmetric and we don't want to deadlock on each side waiting for
    // the other to commit first.
    let mut user_confirm_rx = user_confirm_rx;
    let mut early_confirm = None;
    let our_accept = tokio::select! {
        verdict = &mut user_confirm_rx => verdict.map_err(|_| anyhow!("user confirmation channel dropped"))?,
        message = recv_msg(&mut chan) => {
            let PairMessage::Confirm(confirm) = message? else { bail!("expected peer confirmation"); };
            if !confirm.accepted { bail!("peer declined the pairing"); }
            early_confirm = Some(confirm);
            user_confirm_rx.await.map_err(|_| anyhow!("user confirmation channel dropped"))?
        }
    };
    let sig = if our_accept {
        sign_transcript(&deps.identity.signing_key(), &transcript)
    } else {
        String::new()
    };
    send_msg(
        &mut chan,
        &PairMessage::Confirm(PairConfirm {
            accepted: our_accept,
            sig_hex: sig,
        }),
    )
    .await?;

    if !our_accept {
        bail!("user declined the pairing");
    }

    // ── Step 4: receive peer's Confirm, verify ──────────────────────
    let peer_confirm = match early_confirm {
        Some(confirm) => confirm,
        None => match recv_msg(&mut chan).await? {
            PairMessage::Confirm(c) => c,
            other => bail!("expected Confirm, got {:?}", other),
        },
    };
    if !peer_confirm.accepted {
        bail!("peer declined the pairing");
    }
    verify_signature(&peer_hello.pubkey_hex, &transcript, &peer_confirm.sig_hex)?;

    // ── Step 5: persist + announce ──────────────────────────────────
    let fingerprint =
        synbad_discovery::identity::fingerprint_for(&pubkey_from_hex(&peer_hello.pubkey_hex)?);
    let trusted = TrustedPeer {
        machine_id: peer_hello.machine_id.clone(),
        display_name: peer_hello.display_name.clone(),
        public_key_hex: peer_hello.pubkey_hex.clone(),
        fingerprint,
        paired_at_unix: synbad_discovery::now_unix(),
    };
    {
        let mut store = deps.trust.lock().await;
        store
            .upsert(trusted.clone())
            .context("persisting trusted-peers.json before completing pairing")?;
    }
    let _ = deps.events.send(Event::PairingCompleted { peer: trusted });
    tracing::info!(?session_id, peer = %peer_hello.machine_id, "pairing complete");
    Ok(())
}

async fn send_msg(chan: &mut CipherStream, msg: &PairMessage) -> Result<()> {
    let body = serde_json::to_vec(msg)?;
    tokio::time::timeout(Duration::from_secs(5), chan.send(&body))
        .await
        .map_err(|_| anyhow!("pairing write timed out"))?
        .map_err(|e| anyhow!("encrypted send: {}", e))
}

async fn recv_msg(chan: &mut CipherStream) -> Result<PairMessage> {
    let bytes = chan
        .recv()
        .await
        .map_err(|e| anyhow!("encrypted recv: {}", e))?;
    Ok(serde_json::from_slice(&bytes)?)
}

fn pubkey_from_hex(s: &str) -> Result<[u8; 32]> {
    let bytes = hex::decode(s).map_err(|e| anyhow!("bad pubkey hex: {}", e))?;
    bytes.try_into().map_err(|_| anyhow!("pubkey wrong length"))
}

fn new_session_id() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static N: AtomicU64 = AtomicU64::new(1);
    format!(
        "pair-{}-{}",
        synbad_discovery::now_unix(),
        N.fetch_add(1, Ordering::Relaxed)
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::net::TcpListener;

    async fn check_pairing_persistence(fail_save: bool) {
        let root = std::env::temp_dir().join(format!("synbad-pairing-{}", new_session_id()));
        let mut nodes = Vec::new();
        for name in ["alpha", "beta"] {
            let dir = root.join(name);
            let identity = Arc::new(Identity::load_or_create(&dir.join("identity")).unwrap());
            let path = dir.join("trusted-peers.json");
            let trust = TrustedPeerStore::load(&path).unwrap();
            let (events, rx) = broadcast::channel(16);
            nodes.push((
                Arc::new(SessionDeps {
                    identity,
                    trust: Arc::new(tokio::sync::Mutex::new(trust)),
                    events,
                    display_name: name.into(),
                }),
                rx,
                path,
            ));
        }
        if fail_save {
            std::fs::create_dir(&nodes[0].2).unwrap();
        }
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let outgoing = TcpStream::connect(address).await.unwrap();
        let (incoming, client_addr) = listener.accept().await.unwrap();
        let (accept_a, confirm_a) = oneshot::channel();
        let (accept_b, confirm_b) = oneshot::channel();
        accept_a.send(true).unwrap();
        accept_b.send(true).unwrap();
        let task_a = tokio::spawn(run_session(
            "outbound".into(),
            outgoing,
            address,
            nodes[0].0.clone(),
            confirm_a,
            Some(nodes[1].0.identity.machine_id.to_string()),
        ));
        let task_b = tokio::spawn(run_session(
            "inbound".into(),
            incoming,
            client_addr,
            nodes[1].0.clone(),
            confirm_b,
            None,
        ));
        for (index, (deps, rx, path)) in nodes.iter_mut().enumerate() {
            tokio::time::timeout(Duration::from_secs(5), async {
                loop {
                    match rx.recv().await.unwrap() {
                        Event::PairingCompleted { peer } => {
                            assert!(!(fail_save && index == 0));
                            // Reload immediately when the completion is observed,
                            // without waiting for the session task to finish.
                            let disk = TrustedPeerStore::load(path).unwrap();
                            assert_eq!(
                                disk.get(&peer.machine_id).unwrap().public_key_hex,
                                peer.public_key_hex
                            );
                            break;
                        }
                        Event::PairingFailed { reason, .. } => {
                            assert!(fail_save && index == 0, "{reason}");
                            assert!(reason.contains("persisting trusted-peers.json"));
                            assert!(deps.trust.lock().await.list().is_empty());
                            break;
                        }
                        _ => {}
                    }
                }
            })
            .await
            .unwrap();
        }
        assert_eq!(task_a.await.unwrap().is_err(), fail_save);
        task_b.await.unwrap().unwrap();
        std::fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn pairing_persists_before_announcing_completion() {
        check_pairing_persistence(false).await;
    }

    #[tokio::test]
    async fn pairing_save_failure_never_announces_completion_or_establishes_trust() {
        check_pairing_persistence(true).await;
    }
}
