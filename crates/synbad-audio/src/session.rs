//! Per-peer audio session state machine.
//!
//! One session per paired peer with audio enabled. The session owns:
//! - the WebRTC `RTCPeerConnection`
//! - the local capture stream (mic or loopback) feeding into the outbound track
//! - the local playback stream draining the inbound track
//! - the `synbad_crypto::CipherStream` carrying signaling messages
//!
//! V1 stub: the session struct + lifecycle methods are defined here; the
//! full RTP packetization wiring is layered on top in subsequent commits.
//! The current stub gracefully tears down rather than panicking when the
//! underlying tracks aren't yet plumbed.

use std::sync::Arc;

use synbad_crypto::CipherStream;
use tokio::task::JoinHandle;
use tracing::{debug, info};
use uuid::Uuid;
use webrtc::peer_connection::RTCPeerConnection;

use crate::errors::AudioError;
use crate::rtc;

/// A single negotiated audio link with one peer.
pub struct AudioSession {
    pub session_id: String,
    pub peer_machine_id: String,
    pub peer_connection: Arc<RTCPeerConnection>,
    /// Signaling channel kept alive for the life of the session.
    _signal: CipherStream,
    /// Tasks owning the capture and playback streams.
    _tasks: Vec<JoinHandle<()>>,
}

impl AudioSession {
    /// Create a new session shell. The PeerConnection is built but no
    /// offer/answer has run — callers drive negotiation by reading
    /// `AudioSignal` messages off the signaling channel and feeding them
    /// into the PeerConnection (full SDP/ICE driver pending — see
    /// `docs/AUDIO.md`).
    pub async fn new(peer_machine_id: String, signal: CipherStream) -> Result<Self, AudioError> {
        let api = rtc::build_api()?;
        let pc = rtc::new_peer_connection(&api).await?;
        let session_id = Uuid::new_v4().to_string();
        info!(peer = %peer_machine_id, session = %session_id, "audio session created");
        Ok(Self {
            session_id,
            peer_machine_id,
            peer_connection: pc,
            _signal: signal,
            _tasks: Vec::new(),
        })
    }

    /// Tear down the PeerConnection and stop both audio streams.
    pub async fn close(self, reason: Option<String>) {
        debug!(
            peer = %self.peer_machine_id,
            session = %self.session_id,
            reason = ?reason,
            "audio session closing"
        );
        let _ = self.peer_connection.close().await;
    }
}
