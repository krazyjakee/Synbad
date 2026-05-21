//! Audio signaling transport: TCP listener + outbound dialer.
//!
//! The listener accepts authenticated `CipherStream`s from paired peers
//! and hands each to the `synbad_audio::AudioBridge` to drive an audio
//! session over. The dialer mirrors that path in the opposite direction
//! so a peer that's already trusted starts an audio session as soon as
//! we discover it (subject to the glare rule below).
//!
//! Mirrors the structure of [`crate::sync`] but stays smaller because
//! the bridge owns all session state — this module only does TCP
//! plumbing and the trust-bound handshake.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;
use tracing::{debug, info, warn};

use synbad_audio::{AudioCommand, SessionRole};
use synbad_crypto::{accept as crypto_accept, initiate as crypto_initiate, HandshakeMode};
use synbad_discovery::{Identity, TrustedPeerStore};
use synbad_ipc::DiscoveredPeer;

/// How long a handshake may take before we tear the connection down. The
/// audio bridge can stay open indefinitely once the handshake has
/// completed; this only caps the auth phase.
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(5);

/// How long to wait for the outbound TCP connect itself. The session
/// timeout above kicks in once the bytes start flowing.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(3);

pub struct AudioListenerDeps {
    pub identity: Arc<Identity>,
    pub trust: Arc<tokio::sync::Mutex<TrustedPeerStore>>,
    pub bridge_commands: mpsc::Sender<AudioCommand>,
}

/// Result of an outbound dial reported back to the supervisor so it can
/// clear its in-flight set and bump per-peer backoff. The success arm
/// fires after the bridge has accepted the signaling stream — i.e. the
/// session itself may still be negotiating, but the handshake plumbing
/// is established. The error arm fires on connect/handshake/trust
/// failure.
#[derive(Debug)]
pub enum AudioDialOutcome {
    Ok { peer_machine_id: String },
    Err { peer_machine_id: String, error: String },
}

/// Bind the audio signaling listener on `bind_port`. Returns a JoinHandle
/// for the accept loop — drop it to stop accepting connections.
pub async fn spawn_listener(
    bind_port: u16,
    deps: Arc<AudioListenerDeps>,
) -> Result<tokio::task::JoinHandle<()>> {
    let listener = TcpListener::bind(("0.0.0.0", bind_port))
        .await
        .with_context(|| format!("binding audio signal listener on :{}", bind_port))?;
    info!(port = bind_port, "audio signal listener up");

    let handle = tokio::spawn(async move {
        loop {
            match listener.accept().await {
                Ok((stream, addr)) => {
                    let deps = deps.clone();
                    tokio::spawn(async move {
                        if let Err(e) = handshake_and_handoff(stream, addr, deps).await {
                            debug!(%addr, ?e, "audio signal handshake failed");
                        }
                    });
                }
                Err(e) => {
                    warn!(?e, "audio signal accept failed");
                }
            }
        }
    });
    Ok(handle)
}

/// Dial a paired peer's audio port and hand off the established
/// signaling stream as the [`SessionRole::Offerer`] side.
///
/// Used by the supervisor on `PeerDiscovered` for trusted peers. Caller
/// is responsible for the glare rule (only dial when our `machine_id`
/// sorts lower than the peer's) so two peers don't both end up trying
/// to be offerer.
pub fn spawn_outbound(
    peer: DiscoveredPeer,
    deps: Arc<AudioListenerDeps>,
    outcome_tx: mpsc::Sender<AudioDialOutcome>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let peer_id = peer.machine_id.clone();
        let outcome = match run_outbound(peer.clone(), deps).await {
            Ok(()) => AudioDialOutcome::Ok {
                peer_machine_id: peer_id,
            },
            Err(e) => {
                debug!(
                    peer = %peer.machine_id,
                    ?e,
                    "outbound audio handshake failed"
                );
                AudioDialOutcome::Err {
                    peer_machine_id: peer_id,
                    error: format!("{e:#}"),
                }
            }
        };
        // Best-effort: if the supervisor's receiver is gone we're
        // shutting down anyway.
        let _ = outcome_tx.send(outcome).await;
    })
}

async fn run_outbound(peer: DiscoveredPeer, deps: Arc<AudioListenerDeps>) -> Result<()> {
    if peer.audio_port == 0 {
        bail!("peer {} did not advertise an audio_port", peer.machine_id);
    }
    let trusted = {
        let trust = deps.trust.lock().await;
        trust.get(&peer.machine_id).cloned()
    };
    let trusted = trusted.ok_or_else(|| {
        anyhow!(
            "peer {} is not in the trust store; refusing audio dial",
            peer.machine_id
        )
    })?;

    let peer_pk: [u8; 32] = hex::decode(&trusted.public_key_hex)
        .map_err(|e| anyhow!("trust store pubkey not hex: {}", e))?
        .as_slice()
        .try_into()
        .map_err(|_| anyhow!("trust store pubkey not 32 bytes"))?;

    let addr = format!("{}:{}", peer.host, peer.audio_port);
    let stream = tokio::time::timeout(CONNECT_TIMEOUT, TcpStream::connect(&addr))
        .await
        .map_err(|_| anyhow!("audio connect to {} timed out", addr))??;

    let our_machine_id = deps.identity.machine_id.to_string();
    let signing_key = deps.identity.signing_key();

    let (cipher, _peer_auth) = tokio::time::timeout(
        HANDSHAKE_TIMEOUT,
        crypto_initiate(
            stream,
            HandshakeMode::Authenticated {
                our_signing_key: &signing_key,
                our_machine_id: &our_machine_id,
            },
            Some((trusted.machine_id.as_str(), &peer_pk)),
        ),
    )
    .await
    .map_err(|_| anyhow!("audio handshake to {} timed out", trusted.machine_id))?
    .with_context(|| format!("audio handshake with {}", trusted.machine_id))?;

    info!(peer = %trusted.machine_id, %addr, "audio session dial complete");

    deps.bridge_commands
        .send(AudioCommand::IncomingSignal {
            peer_machine_id: trusted.machine_id,
            stream: cipher,
            role: SessionRole::Offerer,
        })
        .await
        .map_err(|_| anyhow!("audio bridge dropped its command channel"))?;
    Ok(())
}

async fn handshake_and_handoff(
    stream: tokio::net::TcpStream,
    addr: SocketAddr,
    deps: Arc<AudioListenerDeps>,
) -> Result<()> {
    // Snapshot trust to a sync map so the crypto resolver (which is sync)
    // can look peers up without holding the async mutex. Same pattern as
    // `sync::run_inbound_inner`.
    let trust_snapshot: HashMap<String, [u8; 32]> = {
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

    let our_machine_id = deps.identity.machine_id.to_string();
    let signing_key = deps.identity.signing_key();

    let (cipher, peer_auth) = tokio::time::timeout(
        HANDSHAKE_TIMEOUT,
        crypto_accept(
            stream,
            HandshakeMode::Authenticated {
                our_signing_key: &signing_key,
                our_machine_id: &our_machine_id,
            },
            |mid| trust_snapshot.get(mid).copied(),
        ),
    )
    .await
    .map_err(|_| anyhow!("audio handshake from {addr} timed out"))?
    .with_context(|| format!("audio handshake from {addr}"))?;

    let peer_machine_id = match peer_auth {
        synbad_crypto::PeerAuth::Authenticated { machine_id, .. } => machine_id,
        synbad_crypto::PeerAuth::Anonymous => {
            bail!("audio handshake completed anonymously — refusing");
        }
    };

    info!(peer = %peer_machine_id, %addr, "audio session handshake complete");

    deps.bridge_commands
        .send(AudioCommand::IncomingSignal {
            peer_machine_id,
            stream: cipher,
            role: SessionRole::Answerer,
        })
        .await
        .map_err(|_| anyhow!("audio bridge dropped its command channel"))?;
    Ok(())
}
