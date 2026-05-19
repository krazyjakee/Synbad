//! LAN auto-discovery for Synbad.
//!
//! Each `synbadd` advertises a `_synbad._tcp` mDNS service and browses for
//! peers running the same service type. The TXT record carries enough
//! identity to support out-of-band trust establishment (see
//! [DISCOVERY.md](../../docs/DISCOVERY.md)) without exposing private key
//! material.
//!
//! ## Responsibilities
//!
//! - Maintain a **stable per-machine identity**: a persisted UUID + ed25519
//!   keypair so a host keeps the same `id` and fingerprint across reboots,
//!   IP changes, and hostname changes.
//! - Advertise the local node so other peers can find it.
//! - Surface discovered peers as an event stream.
//!
//! ## What this crate **does not** do
//!
//! - Trust / pairing handshake. Discovery only makes peers *visible*; the
//!   GUI must require an explicit user pairing step before a peer is
//!   allowed to participate in input sharing or config sync.
//! - mDNS-blocked-network fallback (manual host:port entry). The GUI
//!   handles that path; this crate only knows about mDNS.

pub mod advertiser;
pub mod browser;
pub mod identity;
pub mod pairing;
pub mod trust;

pub use advertiser::Advertiser;
pub use browser::{Browser, DiscoveryEvent};
pub use identity::{Identity, IdentityError};
pub use pairing::{
    canonical_transcript, sas_code, sign_transcript, verify_signature,
    PairHello, PairConfirm, PairingError,
};
pub use trust::{now_unix, TrustError, TrustedPeer, TrustedPeerStore};

// `DiscoveredPeer` lives in `synbad-ipc` so the GUI can use it without
// transitively compiling mDNS / crypto deps. Re-exported here so callers
// of this crate don't need a separate `synbad-ipc` import.
pub use synbad_ipc::DiscoveredPeer;

/// Service type for our mDNS service. See DISCOVERY.md for the schema.
pub const SERVICE_TYPE: &str = "_synbad._tcp.local.";

/// Discovery protocol version embedded in the TXT record. Bump when the
/// TXT schema changes incompatibly so older peers reject newer records.
pub const PROTOCOL_VERSION: u32 = 1;
