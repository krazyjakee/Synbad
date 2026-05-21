//! Wire protocol for audio-session signaling.
//!
//! These messages flow over a [`synbad_crypto::CipherStream`] established
//! between two paired peers. The stream is already authenticated and
//! AEAD-encrypted; this protocol adds no further crypto.
//!
//! Each message is length-prefixed JSON written via
//! [`synbad_crypto::CipherStream::send`].
//!
//! The session is conceptually long-lived: an [`Offer`](AudioSignal::Offer)
//! starts negotiation, the matching [`Answer`](AudioSignal::Answer)
//! completes it, and a [`Close`](AudioSignal::Close) tears down the
//! session.
//!
//! ## ICE trickling
//!
//! Earlier revisions of this protocol carried `IceCandidate` and
//! `IceComplete` variants for trickle ICE — but the str0m WebRTC
//! stack we use bundles every host candidate into the SDP itself
//! (driven by `add_local_candidate` before `sdp_api().apply()`), so
//! we have no separate candidates to forward. Keeping the protocol
//! small reduces the moving parts and avoids a class of
//! "candidate arrived before remote-description" races.

use serde::{Deserialize, Serialize};

/// One signaling message. The variants line up with WebRTC's standard
/// JSEP exchange — `Offer` / `Answer` carry full SDPs (with ICE
/// candidates already embedded), `Close` lets either side tear the
/// session down without dropping the underlying signaling stream.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum AudioSignal {
    /// SDP offer from the initiator.
    Offer { session_id: String, sdp: String },
    /// SDP answer from the responder.
    Answer { session_id: String, sdp: String },
    /// Tear down this session. The signaling stream stays open.
    Close {
        session_id: String,
        reason: Option<String>,
    },
}

impl AudioSignal {
    /// All variants carry a `session_id`; this accessor saves callers from
    /// matching on every variant.
    pub fn session_id(&self) -> &str {
        match self {
            AudioSignal::Offer { session_id, .. }
            | AudioSignal::Answer { session_id, .. }
            | AudioSignal::Close { session_id, .. } => session_id,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_offer() {
        let signal = AudioSignal::Offer {
            session_id: "abc-123".into(),
            sdp: "v=0\r\no=- ...".into(),
        };
        let bytes = serde_json::to_vec(&signal).unwrap();
        let back: AudioSignal = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(back.session_id(), "abc-123");
    }

    #[test]
    fn round_trip_close_without_reason() {
        let signal = AudioSignal::Close {
            session_id: "s".into(),
            reason: None,
        };
        let bytes = serde_json::to_vec(&signal).unwrap();
        let back: AudioSignal = serde_json::from_slice(&bytes).unwrap();
        assert!(matches!(back, AudioSignal::Close { reason: None, .. }));
    }
}
