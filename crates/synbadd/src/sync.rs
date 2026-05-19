//! Config-sync transport: TCP listener, outbound dialer, per-session loop.
//!
//! The cryptographic/data layer lives in `synbad-sync`; this module is the
//! TCP plumbing that ferries a signed [`SyncFrame`] between two peers and
//! hands the merge work back to the supervisor.
//!
//! ### Why the supervisor owns the merge
//!
//! The supervisor's task loop is the **single writer** of the in-memory
//! config + stamps. If sync sessions mutated state directly, two
//! simultaneous inbound syncs could interleave and lose an update. Instead,
//! a session computes nothing locally: it forwards the peer's state via
//! [`SyncOp::Merge`] and waits for the supervisor to merge and reply with
//! the post-merge state. The supervisor sees every merge serialised, so
//! the reply it sends back is exactly what we should ship to the peer.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use rand_core::RngCore;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{broadcast, mpsc, oneshot};

use synbad_crypto::{
    accept as crypto_accept, initiate as crypto_initiate, CipherStream, HandshakeMode,
};
use synbad_discovery::{Identity, TrustedPeerStore};
use synbad_ipc::{DiscoveredPeer, Event, SyncDirection};
use synbad_sync::{sign_frame, verify_frame, SyncFrame, VersionedConfig};

/// Wall-clock budget for an entire sync session. Sync should be sub-second
/// on a healthy LAN; anything longer is almost certainly a hung peer, and
/// we'd rather drop the connection than keep file descriptors open.
const SESSION_TIMEOUT: Duration = Duration::from_secs(10);

/// Max bytes we'll accept for a single sync frame payload. The encrypted
/// transport caps frames at the same value, so this is a belt-and-braces
/// check that matches the cap a peer can actually push past us.
const MAX_FRAME_BYTES: usize = 256 * 1024;

/// Things every sync session needs. Wrapped in `Arc` so the listener can
/// hand the same set to every accepted connection.
pub struct SyncDeps {
    pub identity: Arc<Identity>,
    pub trust: Arc<tokio::sync::Mutex<TrustedPeerStore>>,
    pub events: broadcast::Sender<Event>,
    /// Sink the session writes to when it needs the supervisor to take an
    /// action on its shared state.
    pub ops: mpsc::Sender<SyncOp>,
}

/// Operations a sync session asks the supervisor to perform on its
/// behalf. The supervisor is single-threaded over its state, so all
/// reads/writes funnel through these requests + oneshot replies.
pub enum SyncOp {
    /// Hand back the current versioned config. Used by the inbound side
    /// to compare heads and to know what state to ship back.
    Snapshot {
        reply: oneshot::Sender<VersionedConfig>,
    },
    /// Merge an incoming peer state into the supervisor's state. The
    /// reply carries the supervisor's *post-merge* state so the session
    /// can ship it back to the peer in the same round trip.
    Merge {
        peer_machine_id: String,
        // Boxed because `VersionedConfig` is the largest field in this
        // enum by a wide margin and the channel-bound variant would
        // otherwise dominate the size of every `SyncOp` slot.
        incoming: Box<VersionedConfig>,
        reply: oneshot::Sender<VersionedConfig>,
    },
}

/// Bind the sync TCP listener. Returns the JoinHandle so the supervisor
/// can keep the task alive — dropping it kills the listener.
pub async fn spawn_listener(
    bind_port: u16,
    deps: Arc<SyncDeps>,
) -> Result<tokio::task::JoinHandle<()>> {
    let listener = TcpListener::bind(("0.0.0.0", bind_port))
        .await
        .with_context(|| format!("binding sync listener on :{}", bind_port))?;
    tracing::info!(port = bind_port, "sync listener up");

    let handle = tokio::spawn(async move {
        loop {
            match listener.accept().await {
                Ok((stream, addr)) => {
                    let deps = deps.clone();
                    tokio::spawn(async move {
                        if let Err(e) = run_session_with_timeout(stream, addr, deps).await {
                            tracing::debug!(%addr, ?e, "sync session ended with error");
                        }
                    });
                }
                Err(e) => {
                    tracing::warn!(?e, "sync accept failed");
                }
            }
        }
    });
    Ok(handle)
}

/// Dial a peer and run an outbound sync session in a background task.
pub fn spawn_outbound(peer: DiscoveredPeer, deps: Arc<SyncDeps>) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        if let Err(e) = run_outbound(peer.clone(), deps.clone()).await {
            tracing::debug!(
                peer = %peer.machine_id,
                ?e,
                "outbound sync failed"
            );
            let _ = deps.events.send(Event::SyncFailed {
                peer_machine_id: peer.machine_id,
                direction: SyncDirection::Outbound,
                reason: e.to_string(),
            });
        }
    })
}

async fn run_outbound(peer: DiscoveredPeer, deps: Arc<SyncDeps>) -> Result<()> {
    if peer.sync_port == 0 {
        bail!("peer {} did not advertise a sync_port", peer.machine_id);
    }
    // Refuse to sync with anyone we haven't paired with. The peer's
    // public key — needed to verify their response — also has to come
    // from the trust store.
    let trusted = {
        let trust = deps.trust.lock().await;
        trust.get(&peer.machine_id).cloned()
    };
    let trusted =
        trusted.ok_or_else(|| anyhow!("peer {} is not in the trust store", peer.machine_id))?;

    let addr = format!("{}:{}", peer.host, peer.sync_port);
    let stream = tokio::time::timeout(Duration::from_secs(3), TcpStream::connect(&addr))
        .await
        .map_err(|_| anyhow!("connect to {} timed out", addr))??;

    let _ = deps.events.send(Event::SyncStarted {
        peer_machine_id: peer.machine_id.clone(),
        direction: SyncDirection::Outbound,
    });

    let result = tokio::time::timeout(
        SESSION_TIMEOUT,
        outbound_inner(stream, deps.clone(), trusted),
    )
    .await
    .map_err(|_| anyhow!("sync session timed out"))??;

    let _ = deps.events.send(Event::SyncCompleted {
        peer_machine_id: peer.machine_id,
        direction: SyncDirection::Outbound,
        updated: result.updated,
        new_head: result.new_head,
    });
    Ok(())
}

struct SessionResult {
    updated: bool,
    new_head: String,
}

async fn outbound_inner(
    stream: TcpStream,
    deps: Arc<SyncDeps>,
    trusted: synbad_discovery::TrustedPeer,
) -> Result<SessionResult> {
    // Authenticated transport handshake — the trust-store entry tells us
    // both who we expect to be talking to and the ed25519 key the
    // encrypted channel must verify against. A peer whose static key
    // doesn't match what we paired with aborts here, before any sync
    // bytes flow.
    let peer_pk: [u8; 32] = hex::decode(&trusted.public_key_hex)
        .map_err(|e| anyhow!("trust store pubkey not hex: {}", e))?
        .as_slice()
        .try_into()
        .map_err(|_| anyhow!("trust store pubkey not 32 bytes"))?;
    let our_machine_id = deps.identity.machine_id.to_string();
    let signing_key = deps.identity.signing_key();
    let (mut chan, _peer) = crypto_initiate(
        stream,
        HandshakeMode::Authenticated {
            our_signing_key: &signing_key,
            our_machine_id: &our_machine_id,
        },
        Some((trusted.machine_id.as_str(), &peer_pk)),
    )
    .await
    .with_context(|| format!("encrypted handshake with {}", trusted.machine_id))?;

    let our_state = snapshot(&deps).await?;

    let mut nonce = [0u8; 16];
    rand_core::OsRng.fill_bytes(&mut nonce);

    let our_frame = sign_frame(&signing_key, &our_machine_id, &nonce, our_state)
        .map_err(|e| anyhow!("sign outbound frame: {}", e))?;
    send_frame(&mut chan, &our_frame).await?;

    let peer_frame = recv_frame(&mut chan).await?;
    if peer_frame.from_machine_id != trusted.machine_id {
        bail!(
            "peer identified as {} but we expected {}",
            peer_frame.from_machine_id,
            trusted.machine_id
        );
    }
    verify_frame(
        &trusted.public_key_hex,
        Some(&hex::encode(nonce)),
        &peer_frame,
    )
    .map_err(|e| anyhow!("verify peer frame: {}", e))?;

    // Hand the peer's state to the supervisor; it merges and returns
    // the new authoritative state.
    let merged = merge(
        &deps,
        peer_frame.from_machine_id.clone(),
        peer_frame.state.clone(),
    )
    .await?;
    let new_head = merged.head_hash();
    // We changed locally iff the merged state's hash differs from the
    // hash of what we sent first. The supervisor knows the previous
    // hash, but we can compute it identically here by hashing
    // `our_frame.state` — they're the same bytes.
    let updated = our_frame.state.head_hash() != new_head;
    Ok(SessionResult { updated, new_head })
}

async fn run_session_with_timeout(
    stream: TcpStream,
    addr: SocketAddr,
    deps: Arc<SyncDeps>,
) -> Result<()> {
    let result = tokio::time::timeout(
        SESSION_TIMEOUT,
        run_inbound_inner(stream, addr, deps.clone()),
    )
    .await;
    match result {
        Ok(Ok(info)) => {
            let _ = deps.events.send(Event::SyncCompleted {
                peer_machine_id: info.peer_machine_id,
                direction: SyncDirection::Inbound,
                updated: info.updated,
                new_head: info.new_head,
            });
            Ok(())
        }
        Ok(Err(e)) => {
            let _ = deps.events.send(Event::SyncFailed {
                peer_machine_id: String::new(),
                direction: SyncDirection::Inbound,
                reason: e.to_string(),
            });
            Err(e)
        }
        Err(_) => {
            let _ = deps.events.send(Event::SyncFailed {
                peer_machine_id: String::new(),
                direction: SyncDirection::Inbound,
                reason: "session timed out".into(),
            });
            bail!("inbound sync session from {} timed out", addr)
        }
    }
}

struct InboundResult {
    peer_machine_id: String,
    updated: bool,
    new_head: String,
}

async fn run_inbound_inner(
    stream: TcpStream,
    addr: SocketAddr,
    deps: Arc<SyncDeps>,
) -> Result<InboundResult> {
    // Snapshot the trust store as (machine_id -> [u8;32] pubkey). The
    // transport handshake's resolver callback is sync, so we can't hold
    // an async mutex inside it; the snapshot is small (one entry per
    // paired peer) and stale-by-microseconds is fine — pairings persist
    // through restarts, not within a single session.
    let trust_snapshot: std::collections::HashMap<String, [u8; 32]> = {
        let trust = deps.trust.lock().await;
        trust
            .list()
            .iter()
            .filter_map(|p| {
                let bytes = hex::decode(&p.public_key_hex).ok()?;
                let arr: [u8; 32] = bytes.as_slice().try_into().ok()?;
                Some((p.machine_id.clone(), arr))
            })
            .collect()
    };

    // Authenticated transport handshake. The resolver looks up the
    // claimed peer in the snapshot; an unknown peer is rejected by
    // `synbad-crypto` before any application bytes are exchanged.
    let our_machine_id = deps.identity.machine_id.to_string();
    let signing_key = deps.identity.signing_key();
    let (mut chan, peer_auth) = crypto_accept(
        stream,
        HandshakeMode::Authenticated {
            our_signing_key: &signing_key,
            our_machine_id: &our_machine_id,
        },
        |mid| trust_snapshot.get(mid).copied(),
    )
    .await
    .with_context(|| format!("encrypted handshake from {}", addr))?;

    let peer_machine_id = match peer_auth {
        synbad_crypto::PeerAuth::Authenticated { machine_id, .. } => machine_id,
        synbad_crypto::PeerAuth::Anonymous => {
            bail!("sync handshake completed anonymously — refusing");
        }
    };

    // Reload the trust entry now that we know who the peer is — we
    // need its display fields for the event payloads. The transport
    // already verified the pubkey, so this lookup is just for metadata.
    let trusted = {
        let trust = deps.trust.lock().await;
        trust.get(&peer_machine_id).cloned().ok_or_else(|| {
            anyhow!(
                "trust entry for {} disappeared mid-session",
                peer_machine_id
            )
        })?
    };

    // Read peer's first frame.
    let peer_frame = recv_frame(&mut chan).await?;
    if peer_frame.from_machine_id != peer_machine_id {
        bail!(
            "transport identity {} ≠ application identity {}",
            peer_machine_id,
            peer_frame.from_machine_id
        );
    }
    verify_frame(&trusted.public_key_hex, None, &peer_frame)
        .map_err(|e| anyhow!("verify inbound frame: {}", e))?;

    let _ = deps.events.send(Event::SyncStarted {
        peer_machine_id: trusted.machine_id.clone(),
        direction: SyncDirection::Inbound,
    });

    // Merge first, then ship back the post-merge state. Doing the merge
    // before the reply means the peer's State frame contains our final
    // converged view, including anything they sent us — both sides end
    // up byte-identical.
    let merged = merge(&deps, trusted.machine_id.clone(), peer_frame.state.clone()).await?;
    let new_head = merged.head_hash();

    let nonce_bytes =
        hex::decode(&peer_frame.nonce_hex).map_err(|_| anyhow!("peer sent non-hex nonce"))?;
    let nonce: [u8; 16] = nonce_bytes
        .as_slice()
        .try_into()
        .map_err(|_| anyhow!("peer nonce was not 16 bytes"))?;
    let reply = sign_frame(&signing_key, &our_machine_id, &nonce, merged.clone())
        .map_err(|e| anyhow!("sign inbound reply: {}", e))?;
    send_frame(&mut chan, &reply).await?;

    // "Updated" here means the merge produced a different head from the
    // state we held before the session. The supervisor sends us the
    // post-merge state but not the pre-merge state, so we approximate
    // with "did the peer's state differ from ours?" — strictly correct
    // for an LWW merge: a no-op merge leaves the head untouched.
    let updated =
        peer_frame.state.head_hash() != new_head || head_of_our_pre_state(&deps).await? != new_head;
    Ok(InboundResult {
        peer_machine_id: trusted.machine_id,
        updated,
        new_head,
    })
}

/// Best-effort post-merge sanity check: read the supervisor's current
/// state again and compare to `new_head`. If it differs, a concurrent
/// local edit landed during our merge — that's fine, the next outgoing
/// push (triggered by that edit) will propagate it.
async fn head_of_our_pre_state(deps: &SyncDeps) -> Result<String> {
    let s = snapshot(deps).await?;
    Ok(s.head_hash())
}

async fn snapshot(deps: &SyncDeps) -> Result<VersionedConfig> {
    let (tx, rx) = oneshot::channel();
    deps.ops
        .send(SyncOp::Snapshot { reply: tx })
        .await
        .map_err(|_| anyhow!("supervisor channel closed"))?;
    rx.await
        .map_err(|_| anyhow!("supervisor dropped snapshot reply"))
}

async fn merge(
    deps: &SyncDeps,
    peer_machine_id: String,
    incoming: VersionedConfig,
) -> Result<VersionedConfig> {
    let (tx, rx) = oneshot::channel();
    deps.ops
        .send(SyncOp::Merge {
            peer_machine_id,
            incoming: Box::new(incoming),
            reply: tx,
        })
        .await
        .map_err(|_| anyhow!("supervisor channel closed"))?;
    rx.await
        .map_err(|_| anyhow!("supervisor dropped merge reply"))
}

async fn send_frame(chan: &mut CipherStream, frame: &SyncFrame) -> Result<()> {
    let body = serde_json::to_vec(frame).context("serializing sync frame")?;
    if body.len() >= MAX_FRAME_BYTES {
        bail!(
            "outbound sync frame is {} bytes (max {})",
            body.len(),
            MAX_FRAME_BYTES
        );
    }
    chan.send(&body)
        .await
        .map_err(|e| anyhow!("encrypted send: {}", e))
}

async fn recv_frame(chan: &mut CipherStream) -> Result<SyncFrame> {
    let body = chan
        .recv()
        .await
        .map_err(|e| anyhow!("encrypted recv: {}", e))?;
    if body.len() >= MAX_FRAME_BYTES {
        bail!(
            "incoming frame is {} bytes (max {})",
            body.len(),
            MAX_FRAME_BYTES
        );
    }
    serde_json::from_slice(&body).context("parsing sync frame")
}
