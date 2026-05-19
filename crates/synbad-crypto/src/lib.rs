//! Authenticated, encrypted transport for Synbad peer connections.
//!
//! Wraps a [`tokio::net::TcpStream`] in an X25519 + ChaCha20-Poly1305
//! channel so a passive observer on the LAN can't read or tamper with
//! config-sync payloads, and trusted peers prove their long-term ed25519
//! identity to each other before the data phase begins.
//!
//! ## Threat model
//!
//! See [`docs/SECURITY.md`](../../docs/SECURITY.md). In short: this layer
//! protects against a passive eavesdropper and an active MITM on the LAN
//! once a pair of peers have completed [pairing][crate]. It does **not**
//! protect against a compromised endpoint (root on either peer can read
//! the trust store and impersonate either side).
//!
//! ## Modes
//!
//! - [`HandshakeMode::Authenticated`] — both sides know each other's
//!   long-term ed25519 public key (e.g. from
//!   [`synbad_discovery::TrustedPeerStore`]). Each side signs the
//!   handshake transcript and verifies the other's signature, so a
//!   third party impersonating a trusted peer is detected before any
//!   data frame is exchanged. This is the mode `synbadd::sync` uses.
//!
//! - [`HandshakeMode::Anonymous`] — no long-term keys are exchanged at
//!   the transport layer. The channel is still encrypted and integrity-
//!   protected against passive observers, but a network MITM that
//!   splices the TCP connection sees plaintext. The pairing flow uses
//!   this mode because trust is bootstrapped at a higher layer (see
//!   [`synbad_discovery::pairing`]); the pairing transcript itself
//!   binds the user-confirmed SAS to the static keys, so a MITM at the
//!   transport layer surfaces as diverging SAS codes on the two
//!   screens.
//!
//! ## Wire format
//!
//! ### Handshake (cleartext, length-prefixed)
//!
//! ```text
//! Initiator → Responder:  [u8 version] [u8 auth_flag]
//!                         [32 B ephemeral X25519 pub]
//!                         [16 B nonce_i]
//! Responder → Initiator:  [32 B ephemeral X25519 pub]
//!                         [16 B nonce_r]
//! ```
//!
//! Each side then computes `shared = X25519(eph_sk, peer_eph_pk)` and
//! `transcript = SHA-256(domain || version || auth_flag || eph_pub_i ||
//! nonce_i || eph_pub_r || nonce_r)`. HKDF-SHA256 with `ikm = shared`
//! and `salt = transcript` produces two 32-byte AEAD keys and two
//! 4-byte nonce prefixes (one per direction).
//!
//! ### Authenticated mode adds, encrypted under the just-derived keys:
//!
//! Both sides send an `AuthFrame` containing `machine_id`,
//! `ed25519_pub_hex`, and `sig_hex` (ed25519 signature over the
//! transcript). The receiver verifies the signature against the
//! expected long-term key — for the initiator, the one it was told to
//! expect; for the responder, the one returned by the resolver
//! callback.
//!
//! ### Data phase
//!
//! `[u32 BE ciphertext_len] [ciphertext || 16 B tag]`. The 12-byte
//! ChaCha20-Poly1305 nonce is `nonce_prefix (4 B) || counter (8 B BE)`
//! where the counter starts at zero and is incremented for every
//! frame in that direction. Frames are capped at
//! [`MAX_FRAME_BYTES`] to bound memory.

mod cipher;
mod handshake;

pub use cipher::{CipherStream, FrameError, MAX_FRAME_BYTES};
pub use handshake::{
    accept, initiate, HandshakeError, HandshakeMode, PeerAuth, TranscriptHash,
};
