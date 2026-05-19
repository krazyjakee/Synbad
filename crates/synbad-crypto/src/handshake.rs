//! X25519 + ed25519 handshake.
//!
//! Returns a [`CipherStream`] ready for the data phase plus the
//! authenticated identity of the peer (or [`PeerAuth::Anonymous`] when
//! the caller asked for an anonymous channel).

use ed25519_dalek::{Signature, Signer, SigningKey, Verifier, VerifyingKey};
use rand_core::{OsRng, RngCore};
use sha2::{Digest, Sha256};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use x25519_dalek::{EphemeralSecret, PublicKey as XPublicKey};

use crate::cipher::CipherStream;

/// Wire protocol version. Bump when handshake or framing layout changes.
const PROTOCOL_VERSION: u8 = 1;

/// Domain separator mixed into the transcript hash so a future protocol
/// with the same primitive shape can't produce identical session keys.
const TRANSCRIPT_DOMAIN: &[u8] = b"synbad-crypto-v1";

/// HKDF info string. Distinct from the transcript domain so a downstream
/// audit can grep for either independently.
const HKDF_INFO: &[u8] = b"synbad-crypto-v1-keys";

/// Hex-encoded ed25519 signatures are 128 chars + change; an auth frame
/// is roughly 280 bytes of JSON. A frame larger than this is junk.
const MAX_AUTH_FRAME_BYTES: usize = 1024;

/// SHA-256 of the handshake transcript. Both peers see the same value
/// after a successful handshake; useful for higher-layer channel
/// binding (e.g. tying the pairing SAS to this channel).
pub type TranscriptHash = [u8; 32];

#[derive(Debug, thiserror::Error)]
pub enum HandshakeError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error(
        "protocol version mismatch: peer sent {0}, expected {}",
        PROTOCOL_VERSION
    )]
    BadVersion(u8),
    #[error("peer signaled an auth mode different from ours")]
    AuthModeMismatch,
    #[error("malformed handshake message: {0}")]
    Malformed(String),
    #[error("peer signature did not verify")]
    BadPeerSignature,
    #[error("peer identified as {got}, expected {expected}")]
    UnexpectedPeer { got: String, expected: String },
    #[error("peer is not in the trust store")]
    UntrustedPeer,
    #[error("auth frame: {0}")]
    AuthDecode(String),
}

/// How both sides of the handshake should authenticate each other.
pub enum HandshakeMode<'a> {
    /// Both sides sign the transcript with their ed25519 key and check
    /// the other side's signature. Used for config sync, where the
    /// peer's long-term key is known from the trust store.
    Authenticated {
        our_signing_key: &'a SigningKey,
        our_machine_id: &'a str,
    },
    /// No identity exchange at the transport layer. Used for pairing,
    /// where trust is bootstrapped at the application layer.
    Anonymous,
}

impl HandshakeMode<'_> {
    fn auth_flag(&self) -> u8 {
        match self {
            HandshakeMode::Authenticated { .. } => 1,
            HandshakeMode::Anonymous => 0,
        }
    }
}

/// Authenticated identity of the peer at the other end of the handshake.
#[derive(Debug, Clone)]
pub enum PeerAuth {
    Authenticated {
        machine_id: String,
        public_key: [u8; 32],
    },
    Anonymous,
}

/// Auth frame exchanged after the symmetric-key phase, in authenticated
/// mode only. JSON because the message is tiny and JSON keeps the wire
/// shape obvious in pcaps captured during development.
#[derive(serde::Serialize, serde::Deserialize)]
struct AuthFrame {
    machine_id: String,
    ed25519_pub_hex: String,
    sig_hex: String,
}

/// Initiate a handshake on `stream`. Returns the encrypted channel and
/// the authenticated peer identity (or `PeerAuth::Anonymous` for
/// `HandshakeMode::Anonymous`).
///
/// `expected_peer` is checked in authenticated mode after the auth
/// frame round-trip: the peer must identify with `(machine_id,
/// public_key)` matching it, or the handshake fails with
/// [`HandshakeError::UnexpectedPeer`]. Pass `None` to accept any
/// authenticated peer (the resolver pattern used on the responder side
/// has no analogue for an outbound dialer that already knows whom it's
/// dialing).
pub async fn initiate(
    mut stream: TcpStream,
    mode: HandshakeMode<'_>,
    expected_peer: Option<(&str, &[u8; 32])>,
) -> Result<(CipherStream, PeerAuth), HandshakeError> {
    let auth_flag = mode.auth_flag();

    // Ephemeral X25519 keypair for this session.
    let our_eph_sk = EphemeralSecret::random_from_rng(OsRng);
    let our_eph_pk = XPublicKey::from(&our_eph_sk);

    let mut nonce_i = [0u8; 16];
    OsRng.fill_bytes(&mut nonce_i);

    // ── Send: version + auth_flag + eph_pk + nonce_i ──────────────────
    let mut hello1 = [0u8; 1 + 1 + 32 + 16];
    hello1[0] = PROTOCOL_VERSION;
    hello1[1] = auth_flag;
    hello1[2..34].copy_from_slice(our_eph_pk.as_bytes());
    hello1[34..50].copy_from_slice(&nonce_i);
    stream.write_all(&hello1).await?;
    stream.flush().await?;

    // ── Recv: eph_pk + nonce_r ────────────────────────────────────────
    let mut hello2 = [0u8; 32 + 16];
    stream.read_exact(&mut hello2).await?;
    let peer_eph_pk: [u8; 32] = hello2[0..32].try_into().unwrap();
    let nonce_r: [u8; 16] = hello2[32..48].try_into().unwrap();

    let (transcript, mut cipher) = finalize_keys(
        /*initiator=*/ true,
        our_eph_sk,
        &peer_eph_pk,
        auth_flag,
        /*init_pub=*/ our_eph_pk.as_bytes(),
        /*resp_pub=*/ &peer_eph_pk,
        &nonce_i,
        &nonce_r,
        stream,
    );

    let peer = match mode {
        HandshakeMode::Authenticated {
            our_signing_key,
            our_machine_id,
        } => {
            // Send our auth frame, then read theirs. The order doesn't
            // matter for security (both sides commit before either reads)
            // but we keep it symmetric so the responder code mirrors this.
            send_auth_frame(&mut cipher, our_signing_key, our_machine_id, &transcript).await?;
            let peer_frame = recv_auth_frame(&mut cipher).await?;
            let peer_pk = verify_auth_frame(&peer_frame, &transcript)?;
            if let Some((want_id, want_pk)) = expected_peer {
                if want_id != peer_frame.machine_id {
                    return Err(HandshakeError::UnexpectedPeer {
                        got: peer_frame.machine_id,
                        expected: want_id.to_string(),
                    });
                }
                if want_pk != &peer_pk {
                    return Err(HandshakeError::BadPeerSignature);
                }
            }
            PeerAuth::Authenticated {
                machine_id: peer_frame.machine_id,
                public_key: peer_pk,
            }
        }
        HandshakeMode::Anonymous => PeerAuth::Anonymous,
    };

    cipher.transcript = transcript;
    Ok((cipher, peer))
}

/// Accept a handshake on `stream`. In authenticated mode, `resolve_pubkey`
/// looks up the expected ed25519 public key for the peer's claimed
/// `machine_id` (returning `None` rejects the peer as untrusted).
pub async fn accept<F>(
    mut stream: TcpStream,
    mode: HandshakeMode<'_>,
    resolve_pubkey: F,
) -> Result<(CipherStream, PeerAuth), HandshakeError>
where
    F: FnOnce(&str) -> Option<[u8; 32]>,
{
    let want_auth_flag = mode.auth_flag();

    // ── Recv hello1 ───────────────────────────────────────────────────
    let mut hello1 = [0u8; 1 + 1 + 32 + 16];
    stream.read_exact(&mut hello1).await?;
    if hello1[0] != PROTOCOL_VERSION {
        return Err(HandshakeError::BadVersion(hello1[0]));
    }
    if hello1[1] != want_auth_flag {
        return Err(HandshakeError::AuthModeMismatch);
    }
    let peer_eph_pk: [u8; 32] = hello1[2..34].try_into().unwrap();
    let nonce_i: [u8; 16] = hello1[34..50].try_into().unwrap();

    let our_eph_sk = EphemeralSecret::random_from_rng(OsRng);
    let our_eph_pk = XPublicKey::from(&our_eph_sk);

    let mut nonce_r = [0u8; 16];
    OsRng.fill_bytes(&mut nonce_r);

    // ── Send hello2 ───────────────────────────────────────────────────
    let mut hello2 = [0u8; 32 + 16];
    hello2[0..32].copy_from_slice(our_eph_pk.as_bytes());
    hello2[32..48].copy_from_slice(&nonce_r);
    stream.write_all(&hello2).await?;
    stream.flush().await?;

    let (transcript, mut cipher) = finalize_keys(
        /*initiator=*/ false,
        our_eph_sk,
        &peer_eph_pk,
        want_auth_flag,
        /*init_pub=*/ &peer_eph_pk,
        /*resp_pub=*/ our_eph_pk.as_bytes(),
        &nonce_i,
        &nonce_r,
        stream,
    );

    let peer = match mode {
        HandshakeMode::Authenticated {
            our_signing_key,
            our_machine_id,
        } => {
            // Symmetric send/recv order — the initiator sends first, so
            // we read first.
            let peer_frame = recv_auth_frame(&mut cipher).await?;
            let peer_pk = verify_auth_frame(&peer_frame, &transcript)?;
            // Trust-store check: the resolver returns the pubkey we
            // expect for this machine_id. A mismatch with what the
            // peer signed → reject.
            let want_pk =
                resolve_pubkey(&peer_frame.machine_id).ok_or(HandshakeError::UntrustedPeer)?;
            if want_pk != peer_pk {
                return Err(HandshakeError::BadPeerSignature);
            }
            send_auth_frame(&mut cipher, our_signing_key, our_machine_id, &transcript).await?;
            PeerAuth::Authenticated {
                machine_id: peer_frame.machine_id,
                public_key: peer_pk,
            }
        }
        HandshakeMode::Anonymous => PeerAuth::Anonymous,
    };

    cipher.transcript = transcript;
    Ok((cipher, peer))
}

/// Compute the transcript hash, derive AEAD keys via HKDF, and hand back
/// a [`CipherStream`] sized for the rest of the session.
///
/// The transcript layout is:
/// ```text
/// SHA-256(
///   "synbad-crypto-v1" || version || auth_flag ||
///   initiator_eph_pub || nonce_i ||
///   responder_eph_pub || nonce_r
/// )
/// ```
/// `initiator_*` and `responder_*` are role-fixed, not "ours/theirs" —
/// both peers feed the same bytes in the same order.
#[allow(clippy::too_many_arguments)]
fn finalize_keys(
    initiator: bool,
    our_eph_sk: EphemeralSecret,
    peer_eph_pk_bytes: &[u8; 32],
    auth_flag: u8,
    init_pub: &[u8; 32],
    resp_pub: &[u8; 32],
    nonce_i: &[u8; 16],
    nonce_r: &[u8; 16],
    stream: TcpStream,
) -> ([u8; 32], CipherStream) {
    let peer_pk = XPublicKey::from(*peer_eph_pk_bytes);
    let shared = our_eph_sk.diffie_hellman(&peer_pk);

    let mut hasher = Sha256::new();
    hasher.update(TRANSCRIPT_DOMAIN);
    hasher.update([PROTOCOL_VERSION, auth_flag]);
    hasher.update(init_pub);
    hasher.update(nonce_i);
    hasher.update(resp_pub);
    hasher.update(nonce_r);
    let transcript: [u8; 32] = hasher.finalize().into();

    // 64 bytes of AEAD key + 8 bytes of nonce prefix split across the
    // two directions. HKDF-SHA256 of a 32-byte secret with a 32-byte
    // salt is plenty.
    let mut okm = [0u8; 32 + 32 + 4 + 4];
    let hk = hkdf::Hkdf::<Sha256>::new(Some(&transcript), shared.as_bytes());
    hk.expand(HKDF_INFO, &mut okm)
        .expect("HKDF-SHA256 expand within bounds");
    let mut k_i_to_r = [0u8; 32];
    let mut k_r_to_i = [0u8; 32];
    let mut np_i_to_r = [0u8; 4];
    let mut np_r_to_i = [0u8; 4];
    k_i_to_r.copy_from_slice(&okm[0..32]);
    k_r_to_i.copy_from_slice(&okm[32..64]);
    np_i_to_r.copy_from_slice(&okm[64..68]);
    np_r_to_i.copy_from_slice(&okm[68..72]);

    let (send_key, recv_key, send_prefix, recv_prefix) = if initiator {
        (k_i_to_r, k_r_to_i, np_i_to_r, np_r_to_i)
    } else {
        (k_r_to_i, k_i_to_r, np_r_to_i, np_i_to_r)
    };

    let cipher = CipherStream::new(stream, send_key, recv_key, send_prefix, recv_prefix);
    (transcript, cipher)
}

async fn send_auth_frame(
    cipher: &mut CipherStream,
    our_signing_key: &SigningKey,
    our_machine_id: &str,
    transcript: &[u8; 32],
) -> Result<(), HandshakeError> {
    let sig: Signature = our_signing_key.sign(transcript);
    let frame = AuthFrame {
        machine_id: our_machine_id.to_string(),
        ed25519_pub_hex: hex::encode(our_signing_key.verifying_key().to_bytes()),
        sig_hex: hex::encode(sig.to_bytes()),
    };
    let body = serde_json::to_vec(&frame).map_err(|e| HandshakeError::Malformed(e.to_string()))?;
    cipher
        .send(&body)
        .await
        .map_err(|e| HandshakeError::AuthDecode(e.to_string()))?;
    Ok(())
}

async fn recv_auth_frame(cipher: &mut CipherStream) -> Result<AuthFrame, HandshakeError> {
    let bytes = cipher
        .recv()
        .await
        .map_err(|e| HandshakeError::AuthDecode(e.to_string()))?;
    if bytes.len() > MAX_AUTH_FRAME_BYTES {
        return Err(HandshakeError::Malformed(format!(
            "auth frame is {} bytes (max {})",
            bytes.len(),
            MAX_AUTH_FRAME_BYTES
        )));
    }
    serde_json::from_slice(&bytes).map_err(|e| HandshakeError::AuthDecode(e.to_string()))
}

fn verify_auth_frame(frame: &AuthFrame, transcript: &[u8; 32]) -> Result<[u8; 32], HandshakeError> {
    let pk_bytes = hex::decode(&frame.ed25519_pub_hex)
        .map_err(|_| HandshakeError::Malformed("auth ed25519_pub_hex".into()))?;
    let pk_array: [u8; 32] = pk_bytes
        .as_slice()
        .try_into()
        .map_err(|_| HandshakeError::Malformed("ed25519 key wrong length".into()))?;
    let vk = VerifyingKey::from_bytes(&pk_array)
        .map_err(|_| HandshakeError::Malformed("ed25519 key not on curve".into()))?;

    let sig_bytes = hex::decode(&frame.sig_hex)
        .map_err(|_| HandshakeError::Malformed("auth sig_hex".into()))?;
    let sig_array: [u8; 64] = sig_bytes
        .as_slice()
        .try_into()
        .map_err(|_| HandshakeError::Malformed("sig wrong length".into()))?;
    let sig = Signature::from_bytes(&sig_array);

    vk.verify(transcript, &sig)
        .map_err(|_| HandshakeError::BadPeerSignature)?;
    Ok(pk_array)
}

#[cfg(test)]
mod tests {
    use super::*;
    use ed25519_dalek::SigningKey;
    use rand_core::OsRng;
    use tokio::net::{TcpListener, TcpStream};

    async fn pair_sockets() -> (TcpStream, TcpStream) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let connect = tokio::spawn(async move { TcpStream::connect(addr).await.unwrap() });
        let (server, _) = listener.accept().await.unwrap();
        let client = connect.await.unwrap();
        (client, server)
    }

    fn signing_key() -> SigningKey {
        SigningKey::generate(&mut OsRng)
    }

    #[tokio::test]
    async fn anonymous_handshake_roundtrip() {
        let (client, server) = pair_sockets().await;
        let init = tokio::spawn(async move {
            let (mut s, peer) = initiate(client, HandshakeMode::Anonymous, None)
                .await
                .unwrap();
            assert!(matches!(peer, PeerAuth::Anonymous));
            s.send(b"hello server").await.unwrap();
            let resp = s.recv().await.unwrap();
            assert_eq!(resp, b"hello client");
        });
        let acc = tokio::spawn(async move {
            let (mut s, peer) = accept(server, HandshakeMode::Anonymous, |_| None)
                .await
                .unwrap();
            assert!(matches!(peer, PeerAuth::Anonymous));
            let req = s.recv().await.unwrap();
            assert_eq!(req, b"hello server");
            s.send(b"hello client").await.unwrap();
        });
        init.await.unwrap();
        acc.await.unwrap();
    }

    #[tokio::test]
    async fn authenticated_handshake_roundtrip() {
        let sk_init = signing_key();
        let sk_resp = signing_key();
        let pk_init: [u8; 32] = sk_init.verifying_key().to_bytes();
        let pk_resp: [u8; 32] = sk_resp.verifying_key().to_bytes();

        let (client, server) = pair_sockets().await;

        let init_task = {
            let sk = sk_init.clone();
            tokio::spawn(async move {
                let (mut s, peer) = initiate(
                    client,
                    HandshakeMode::Authenticated {
                        our_signing_key: &sk,
                        our_machine_id: "init",
                    },
                    Some(("resp", &pk_resp)),
                )
                .await
                .unwrap();
                match peer {
                    PeerAuth::Authenticated {
                        machine_id,
                        public_key,
                    } => {
                        assert_eq!(machine_id, "resp");
                        assert_eq!(public_key, pk_resp);
                    }
                    _ => panic!("expected authenticated"),
                }
                s.send(b"ping").await.unwrap();
                let r = s.recv().await.unwrap();
                assert_eq!(r, b"pong");
            })
        };

        let acc_task = {
            let sk = sk_resp.clone();
            tokio::spawn(async move {
                let (mut s, peer) = accept(
                    server,
                    HandshakeMode::Authenticated {
                        our_signing_key: &sk,
                        our_machine_id: "resp",
                    },
                    move |mid| {
                        if mid == "init" {
                            Some(pk_init)
                        } else {
                            None
                        }
                    },
                )
                .await
                .unwrap();
                match peer {
                    PeerAuth::Authenticated {
                        machine_id,
                        public_key,
                    } => {
                        assert_eq!(machine_id, "init");
                        assert_eq!(public_key, pk_init);
                    }
                    _ => panic!("expected authenticated"),
                }
                let r = s.recv().await.unwrap();
                assert_eq!(r, b"ping");
                s.send(b"pong").await.unwrap();
            })
        };

        init_task.await.unwrap();
        acc_task.await.unwrap();
    }

    #[tokio::test]
    async fn unknown_peer_is_rejected() {
        let sk_init = signing_key();
        let sk_resp = signing_key();
        let (client, server) = pair_sockets().await;

        let init = tokio::spawn(async move {
            let _ = initiate(
                client,
                HandshakeMode::Authenticated {
                    our_signing_key: &sk_init,
                    our_machine_id: "stranger",
                },
                None,
            )
            .await;
        });
        let result = accept(
            server,
            HandshakeMode::Authenticated {
                our_signing_key: &sk_resp,
                our_machine_id: "resp",
            },
            |_mid| None,
        )
        .await;
        assert!(matches!(result, Err(HandshakeError::UntrustedPeer)));
        let _ = init.await;
    }

    #[tokio::test]
    async fn mode_mismatch_is_rejected() {
        let (client, server) = pair_sockets().await;
        let init = tokio::spawn(async move {
            let _ = initiate(client, HandshakeMode::Anonymous, None).await;
        });
        let sk = signing_key();
        let result = accept(
            server,
            HandshakeMode::Authenticated {
                our_signing_key: &sk,
                our_machine_id: "x",
            },
            |_| None,
        )
        .await;
        assert!(matches!(result, Err(HandshakeError::AuthModeMismatch)));
        let _ = init.await;
    }
}
