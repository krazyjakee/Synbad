//! Pairing protocol — transport-agnostic pieces.
//!
//! Two peers establish mutual trust by:
//! 1. Exchanging `PairHello` messages (each side's ed25519 public key, a
//!    fresh nonce, machine_id, and display_name).
//! 2. Computing a [canonical transcript](canonical_transcript) over both
//!    sides' contributions — the canonical form is independent of who
//!    initiated, so both peers derive the **same** transcript.
//! 3. Each side signs the transcript with its ed25519 private key and
//!    shows a [SAS code](sas_code) derived from the same transcript to
//!    its user.
//! 4. After the user confirms (out-of-band — typically by visually
//!    comparing the SAS code on both screens), each side sends
//!    `PairConfirm { accepted: true, sig_hex }`.
//! 5. Each side verifies the other's signature against the received
//!    public key. If both `accepted` and both signatures verify, trust
//!    is established and the peer is persisted via
//!    [`TrustedPeerStore`](crate::TrustedPeerStore).
//!
//! ### Why this resists a network MITM
//!
//! An attacker that splices the TCP connection performs two independent
//! handshakes (one with each side). The two halves have different public
//! keys, so the SAS codes diverge — the user comparing codes catches the
//! attack. The signed transcript binds the SAS to the exchanged
//! public keys, so the attacker can't make the codes match by replaying
//! signatures.
//!
//! This module is transport-agnostic: it defines messages and the
//! transcript algebra. Network plumbing lives in `synbadd::pairing`.

use ed25519_dalek::{Signature, SigningKey, Verifier, VerifyingKey};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// Domain-separator string mixed into the SAS so a future protocol with
/// the same transcript shape can't produce identical codes.
const SAS_DOMAIN: &[u8] = b"synbad-pair-sas-v1";

#[derive(Debug, thiserror::Error)]
pub enum PairingError {
    #[error("malformed message: {0}")]
    Malformed(String),
    #[error("invalid signature from peer")]
    BadSignature,
    #[error("peer declined the pairing")]
    PeerDeclined,
    #[error("invalid public key from peer")]
    BadPublicKey,
}

/// First message each side sends after the TCP connection opens. There is
/// no client/server asymmetry — the responder sends the same shape.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PairHello {
    /// Hex-encoded ed25519 public key (64 chars).
    pub pubkey_hex: String,
    /// Hex-encoded random nonce — 16 bytes is enough to make replay
    /// impossible.
    pub nonce_hex: String,
    pub machine_id: String,
    pub display_name: String,
}

/// Sent after the user has confirmed (or declined) the pairing.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PairConfirm {
    pub accepted: bool,
    /// Hex-encoded ed25519 signature over the canonical transcript.
    /// Senders that decline (`accepted = false`) MAY send an empty
    /// signature — the receiver should not require the signature unless
    /// the peer also said yes.
    pub sig_hex: String,
}

/// Build the canonical transcript from both sides' Hello payloads.
///
/// The transcript is order-independent (the lexicographically smaller
/// pubkey comes first), so both peers — regardless of who initiated —
/// compute the same byte sequence.
///
/// Layout (all length-prefixed with a 4-byte big-endian u32):
/// ```text
/// [pubkey_lo] [nonce_lo] [machine_id_lo] [display_name_lo]
/// [pubkey_hi] [nonce_hi] [machine_id_hi] [display_name_hi]
/// ```
pub fn canonical_transcript(a: &PairHello, b: &PairHello) -> Vec<u8> {
    // Order by the raw public key bytes.
    let (lo, hi) = if a.pubkey_hex.as_bytes() <= b.pubkey_hex.as_bytes() {
        (a, b)
    } else {
        (b, a)
    };
    let mut out = Vec::with_capacity(512);
    for side in [lo, hi] {
        push_field(&mut out, side.pubkey_hex.as_bytes());
        push_field(&mut out, side.nonce_hex.as_bytes());
        push_field(&mut out, side.machine_id.as_bytes());
        push_field(&mut out, side.display_name.as_bytes());
    }
    out
}

fn push_field(out: &mut Vec<u8>, field: &[u8]) {
    let len = (field.len() as u32).to_be_bytes();
    out.extend_from_slice(&len);
    out.extend_from_slice(field);
}

/// Derive the user-facing Short Authentication String from the transcript.
///
/// Returns a hyphenated lowercase hex string like `"ab-cd-ef-12-34-56"` —
/// 6 bytes = 48 bits of entropy, enough that an MITM has a 1 in 2^48
/// chance of producing the same code by accident.
pub fn sas_code(transcript: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(transcript);
    hasher.update(SAS_DOMAIN);
    let digest = hasher.finalize();
    let bytes = &digest[..6];
    let hex_chars: String = hex::encode(bytes).chars().collect();
    // Format: "ab-cd-ef-12-34-56" — pairs separated by hyphens.
    let mut out = String::with_capacity(17);
    for (i, c) in hex_chars.chars().enumerate() {
        if i > 0 && i % 2 == 0 {
            out.push('-');
        }
        out.push(c);
    }
    out
}

pub fn sign_transcript(signing_key: &SigningKey, transcript: &[u8]) -> String {
    use ed25519_dalek::Signer;
    let sig: Signature = signing_key.sign(transcript);
    hex::encode(sig.to_bytes())
}

/// Verify a hex-encoded signature against a hex-encoded public key.
pub fn verify_signature(
    peer_pubkey_hex: &str,
    transcript: &[u8],
    sig_hex: &str,
) -> Result<(), PairingError> {
    let pk_bytes = hex::decode(peer_pubkey_hex).map_err(|_| PairingError::BadPublicKey)?;
    let pk_array: [u8; 32] = pk_bytes
        .as_slice()
        .try_into()
        .map_err(|_| PairingError::BadPublicKey)?;
    let pk = VerifyingKey::from_bytes(&pk_array).map_err(|_| PairingError::BadPublicKey)?;

    let sig_bytes = hex::decode(sig_hex).map_err(|_| PairingError::BadSignature)?;
    let sig_array: [u8; 64] = sig_bytes
        .as_slice()
        .try_into()
        .map_err(|_| PairingError::BadSignature)?;
    let sig = Signature::from_bytes(&sig_array);

    pk.verify(transcript, &sig).map_err(|_| PairingError::BadSignature)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fresh_pair() -> (SigningKey, SigningKey) {
        let mut rng = rand_core::OsRng;
        (SigningKey::generate(&mut rng), SigningKey::generate(&mut rng))
    }

    fn hello(sk: &SigningKey, machine_id: &str, name: &str, nonce: &str) -> PairHello {
        PairHello {
            pubkey_hex: hex::encode(sk.verifying_key().to_bytes()),
            nonce_hex: nonce.into(),
            machine_id: machine_id.into(),
            display_name: name.into(),
        }
    }

    #[test]
    fn transcript_is_order_independent() {
        let (a, b) = fresh_pair();
        let ha = hello(&a, "a-id", "Alice", "1111");
        let hb = hello(&b, "b-id", "Bob", "2222");
        assert_eq!(canonical_transcript(&ha, &hb), canonical_transcript(&hb, &ha));
    }

    #[test]
    fn sas_matches_both_sides_for_same_transcript() {
        let (a, b) = fresh_pair();
        let ha = hello(&a, "a-id", "Alice", "1111");
        let hb = hello(&b, "b-id", "Bob", "2222");
        let t = canonical_transcript(&ha, &hb);
        assert_eq!(sas_code(&t), sas_code(&canonical_transcript(&hb, &ha)));
    }

    #[test]
    fn sas_differs_when_a_pubkey_changes_mitm_scenario() {
        let (a, b) = fresh_pair();
        let (_, attacker) = fresh_pair();

        let ha = hello(&a, "a-id", "Alice", "1111");
        let hb = hello(&b, "b-id", "Bob", "2222");
        let h_atk = hello(&attacker, "atk-id", "Bob", "2222");

        // Alice would compute SAS thinking she's talking to Bob, but really
        // her TCP got spliced and she sees the attacker's pubkey.
        let alice_view = canonical_transcript(&ha, &h_atk);
        let bob_view = canonical_transcript(&hb, &h_atk);
        // The codes diverge — that's exactly what the user notices.
        assert_ne!(sas_code(&alice_view), sas_code(&bob_view));
    }

    #[test]
    fn signature_roundtrip() {
        let (a, _b) = fresh_pair();
        let t = b"transcript bytes here";
        let sig = sign_transcript(&a, t);
        verify_signature(
            &hex::encode(a.verifying_key().to_bytes()),
            t,
            &sig,
        )
        .unwrap();
    }

    #[test]
    fn signature_rejects_wrong_pubkey() {
        let (a, b) = fresh_pair();
        let t = b"transcript bytes here";
        let sig = sign_transcript(&a, t);
        assert!(verify_signature(
            &hex::encode(b.verifying_key().to_bytes()),
            t,
            &sig,
        )
        .is_err());
    }
}
