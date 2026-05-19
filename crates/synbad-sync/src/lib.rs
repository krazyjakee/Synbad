//! LAN config sync for Synbad.
//!
//! Replicates the [`Config`](synbad_config::Config) document across all
//! trusted peers on the LAN. Conflict resolution is per-field
//! Last-Write-Wins keyed by a Lamport-style `(counter, machine_id)`
//! timestamp (see [CONFIG-SYNC.md](../../docs/CONFIG-SYNC.md)). Merges are
//! deterministic and commutative, so all trusted peers converge to the
//! same state regardless of which one accepted an edit.
//!
//! ## Layers
//!
//! | Module       | Responsibility                                            |
//! |--------------|-----------------------------------------------------------|
//! | [`versioned`]| `VersionedConfig`: stamps + LWW merge + canonical hashing |
//! | [`protocol`] | Wire messages, signing, and verification                  |
//!
//! Transport plumbing (TCP listener / outbound dialer / session orchestration)
//! lives in `synbadd::sync`. This crate only owns the data model and the
//! cryptographic message envelope so the GUI / config crates can depend on
//! it without pulling in tokio TCP code.

pub mod protocol;
pub mod versioned;

pub use protocol::{
    canonical_state_bytes, sign_frame, verify_frame, ProtocolError, SyncFrame,
};
pub use versioned::{
    FieldStamps, LamportTime, MergeOutcome, VersionedConfig, VersionedConfigError,
};
