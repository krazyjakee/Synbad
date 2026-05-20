//! Audio signaling transport: TCP listener that accepts authenticated
//! `CipherStream`s from paired peers and hands each to the
//! `synbad_audio::AudioBridge` to drive an audio session over.
//!
//! Mirrors the structure of [`crate::sync`] but stays much smaller because
//! the bridge owns all session state — this module only does TCP plumbing
//! and the trust-bound handshake.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use tokio::net::TcpListener;
use tokio::sync::mpsc;
use tracing::{debug, info, warn};

use synbad_audio::AudioCommand;
use synbad_crypto::{accept as crypto_accept, HandshakeMode};
use synbad_discovery::{Identity, TrustedPeerStore};

/// How long a handshake may take before we tear the connection down. The
/// audio bridge can stay open indefinitely once the handshake has
/// completed; this only caps the auth phase.
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(5);

pub struct AudioListenerDeps {
    pub identity: Arc<Identity>,
    pub trust: Arc<tokio::sync::Mutex<TrustedPeerStore>>,
    pub bridge_commands: mpsc::Sender<AudioCommand>,
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
        })
        .await
        .map_err(|_| anyhow!("audio bridge dropped its command channel"))?;
    Ok(())
}
