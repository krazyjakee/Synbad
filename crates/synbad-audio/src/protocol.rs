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
//! completes it, and zero-or-more [`IceCandidate`](AudioSignal::IceCandidate)
//! messages trickle in afterwards as ICE gathering completes. A
//! [`Close`](AudioSignal::Close) tears down the WebRTC PeerConnection
//! without dropping the signaling channel — useful for renegotiation.

use serde::{Deserialize, Serialize};

/// One signaling message. The variants line up with WebRTC's standard
/// JSEP exchange.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum AudioSignal {
    /// SDP offer from the initiator.
    Offer { session_id: String, sdp: String },
    /// SDP answer from the responder.
    Answer { session_id: String, sdp: String },
    /// Trickled ICE candidate.
    IceCandidate {
        session_id: String,
        candidate: String,
        sdp_mid: Option<String>,
        sdp_mline_index: Option<u16>,
    },
    /// End-of-candidates marker (null candidate per RFC 8838).
    IceComplete { session_id: String },
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
            | AudioSignal::IceCandidate { session_id, .. }
            | AudioSignal::IceComplete { session_id }
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
    fn round_trip_ice() {
        let signal = AudioSignal::IceCandidate {
            session_id: "s".into(),
            candidate: "candidate:1 1 UDP 2113937151 192.168.1.5 54321 typ host".into(),
            sdp_mid: Some("0".into()),
            sdp_mline_index: Some(0),
        };
        let bytes = serde_json::to_vec(&signal).unwrap();
        let back: AudioSignal = serde_json::from_slice(&bytes).unwrap();
        match back {
            AudioSignal::IceCandidate { candidate, .. } => assert!(candidate.contains("host")),
            _ => panic!("variant changed in round-trip"),
        }
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
