//! Sync wire protocol.
//!
//! Two trusted peers converge their [`VersionedConfig`] by exchanging
//! exactly one signed frame in each direction:
//!
//! ```text
//!   Initiator                                       Listener
//!     │                                                │
//!     │ ── SyncFrame { nonce_I, state_I, sig_I } ───▶ │
//!     │                                                │  verify_frame(initiator pubkey)
//!     │                                                │  merge state_I into local
//!     │ ◀── SyncFrame { nonce_I (echo), state_L, sig_L }
//!     │                                                │
//!   verify, merge                                      │
//! ```
//!
//! `state_*` is each side's [`VersionedConfig`] *at the moment of
//! transmission*. The listener's response carries its post-merge state, so
//! the initiator picks up anything the listener happened to be advancing
//! locally at the same time.
//!
//! `nonce_I` is the initiator's fresh 16-byte random nonce. The listener
//! echoes it in its response, and both signatures cover the nonce — that
//! binds each message to *this* TCP connection and stops a replayed
//! signature from being reused in a future session.
//!
//! ### Auth model
//!
//! Both signatures use ed25519 over a canonical byte string. The verifier
//! looks the sender's public key up in
//! [`TrustedPeerStore`](synbad_discovery::TrustedPeerStore) keyed by
//! `from_machine_id`. A peer that isn't already paired is rejected before
//! its state is read, so a hostile LAN node can't poison anyone's config.
//!
//! ### Why a fixed-shape canonical signing string
//!
//! `serde_json` field order is declaration-order on structs and key-sorted
//! on `BTreeMap`, so the canonical JSON we hash for signing is stable
//! across implementations *of the same Rust code*. To stay robust across
//! schema evolution, the signing transcript also length-prefixes each
//! component so an attacker can't sneak field-boundary changes past the
//! signature.

use ed25519_dalek::{Signature, Signer, SigningKey, Verifier, VerifyingKey};
use serde::{Deserialize, Serialize};

use crate::versioned::VersionedConfig;

const SIGN_DOMAIN: &[u8] = b"synbad-sync-v1";

#[derive(Debug, thiserror::Error)]
pub enum ProtocolError {
    #[error("malformed: {0}")]
    Malformed(String),
    #[error("signature did not verify")]
    BadSignature,
    #[error("public key in trust store is malformed")]
    BadPublicKey,
    #[error("hex decode: {0}")]
    Hex(#[from] hex::FromHexError),
    #[error("json: {0}")]
    Json(#[from] serde_json::Error),
}

/// A single signed sync frame. Both initiator and listener send exactly
/// one of these per session.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SyncFrame {
    /// Sender's stable machine_id. The receiver looks this up in its
    /// trust store to find the ed25519 public key to verify against.
    pub from_machine_id: String,
    /// 16-byte nonce, hex-encoded. The initiator picks a fresh one; the
    /// listener echoes the initiator's nonce in its response.
    pub nonce_hex: String,
    /// Sender's current versioned config.
    pub state: VersionedConfig,
    /// Hex-encoded ed25519 signature over [`canonical_state_bytes`].
    pub sig_hex: String,
}

/// Build the byte string an ed25519 signature covers.
///
/// Layout (each component length-prefixed with a 4-byte big-endian u32):
/// ```text
/// [SIGN_DOMAIN] [from_machine_id] [nonce_bytes] [canonical_json(state)]
/// ```
pub fn canonical_state_bytes(
    from_machine_id: &str,
    nonce: &[u8],
    state: &VersionedConfig,
) -> Result<Vec<u8>, ProtocolError> {
    let state_bytes = serde_json::to_vec(state)?;
    let mut out = Vec::with_capacity(state_bytes.len() + 64);
    push_field(&mut out, SIGN_DOMAIN);
    push_field(&mut out, from_machine_id.as_bytes());
    push_field(&mut out, nonce);
    push_field(&mut out, &state_bytes);
    Ok(out)
}

fn push_field(out: &mut Vec<u8>, field: &[u8]) {
    out.extend_from_slice(&(field.len() as u32).to_be_bytes());
    out.extend_from_slice(field);
}

/// Sign `(machine_id, nonce, state)` with our ed25519 key and return the
/// fully-formed [`SyncFrame`].
pub fn sign_frame(
    signing_key: &SigningKey,
    from_machine_id: &str,
    nonce: &[u8; 16],
    state: VersionedConfig,
) -> Result<SyncFrame, ProtocolError> {
    let signed_bytes = canonical_state_bytes(from_machine_id, nonce, &state)?;
    let sig: Signature = signing_key.sign(&signed_bytes);
    Ok(SyncFrame {
        from_machine_id: from_machine_id.to_string(),
        nonce_hex: hex::encode(nonce),
        state,
        sig_hex: hex::encode(sig.to_bytes()),
    })
}

/// Verify a [`SyncFrame`] against the sender's hex-encoded public key. The
/// caller looks the public key up in the trust store; this function only
/// validates the cryptographic binding.
pub fn verify_frame(
    peer_pubkey_hex: &str,
    expected_nonce_hex: Option<&str>,
    frame: &SyncFrame,
) -> Result<(), ProtocolError> {
    if let Some(expected) = expected_nonce_hex {
        if expected != frame.nonce_hex {
            return Err(ProtocolError::Malformed("nonce echo mismatch".into()));
        }
    }
    let nonce = hex::decode(&frame.nonce_hex)?;
    let signed_bytes = canonical_state_bytes(&frame.from_machine_id, &nonce, &frame.state)?;

    let pk_bytes = hex::decode(peer_pubkey_hex).map_err(|_| ProtocolError::BadPublicKey)?;
    let pk_array: [u8; 32] = pk_bytes
        .as_slice()
        .try_into()
        .map_err(|_| ProtocolError::BadPublicKey)?;
    let verifying = VerifyingKey::from_bytes(&pk_array).map_err(|_| ProtocolError::BadPublicKey)?;

    let sig_bytes = hex::decode(&frame.sig_hex).map_err(|_| ProtocolError::BadSignature)?;
    let sig_array: [u8; 64] = sig_bytes
        .as_slice()
        .try_into()
        .map_err(|_| ProtocolError::BadSignature)?;
    let sig = Signature::from_bytes(&sig_array);

    verifying
        .verify(&signed_bytes, &sig)
        .map_err(|_| ProtocolError::BadSignature)
}

#[cfg(test)]
mod tests {
    use super::*;
    use ed25519_dalek::SigningKey;
    use rand_core::OsRng;
    use synbad_config::Config;

    fn keypair() -> SigningKey {
        SigningKey::generate(&mut OsRng)
    }

    #[test]
    fn sign_and_verify_roundtrip() {
        let key = keypair();
        let pub_hex = hex::encode(key.verifying_key().to_bytes());
        let nonce = [7u8; 16];
        let state = VersionedConfig::initial(Config::default(), "alpha-id");
        let frame = sign_frame(&key, "alpha-id", &nonce, state).unwrap();
        verify_frame(&pub_hex, None, &frame).unwrap();
        verify_frame(&pub_hex, Some(&hex::encode(nonce)), &frame).unwrap();
    }

    #[test]
    fn tampered_state_fails_verification() {
        let key = keypair();
        let pub_hex = hex::encode(key.verifying_key().to_bytes());
        let nonce = [0u8; 16];
        let state = VersionedConfig::initial(Config::default(), "alpha-id");
        let mut frame = sign_frame(&key, "alpha-id", &nonce, state).unwrap();

        // Mutate the embedded config — signature should now fail.
        frame.state.config.server_name = "evil".into();
        assert!(matches!(
            verify_frame(&pub_hex, None, &frame),
            Err(ProtocolError::BadSignature)
        ));
    }

    #[test]
    fn wrong_pubkey_fails_verification() {
        let key = keypair();
        let other = keypair();
        let nonce = [0u8; 16];
        let state = VersionedConfig::initial(Config::default(), "alpha-id");
        let frame = sign_frame(&key, "alpha-id", &nonce, state).unwrap();
        assert!(matches!(
            verify_frame(&hex::encode(other.verifying_key().to_bytes()), None, &frame),
            Err(ProtocolError::BadSignature)
        ));
    }

    #[test]
    fn echo_nonce_mismatch_is_rejected() {
        let key = keypair();
        let pub_hex = hex::encode(key.verifying_key().to_bytes());
        let nonce = [1u8; 16];
        let state = VersionedConfig::initial(Config::default(), "alpha-id");
        let frame = sign_frame(&key, "alpha-id", &nonce, state).unwrap();
        let wrong_echo = hex::encode([2u8; 16]);
        assert!(matches!(
            verify_frame(&pub_hex, Some(&wrong_echo), &frame),
            Err(ProtocolError::Malformed(_))
        ));
    }
}
