//! WebRTC PeerConnection setup for audio bridging.
//!
//! Wires up:
//! - A `MediaEngine` that advertises raw L16 mono 48 kHz audio (RFC 3551
//!   payload type 11 reused as a dynamic PT; no Opus, no AEC).
//! - A single audio-only RTCRtpTransceiver in `sendrecv` mode.
//! - An empty `ICEServers` list — we're LAN-only and rely on host
//!   candidates the OS already advertises.
//!
//! The `MediaEngine` API in webrtc 0.17 doesn't ship an L16 codec preset;
//! we register a custom `RTCRtpCodecCapability` matching `audio/L16`.

use std::sync::Arc;

use webrtc::api::interceptor_registry::register_default_interceptors;
use webrtc::api::media_engine::MediaEngine;
use webrtc::api::{APIBuilder, API};
use webrtc::interceptor::registry::Registry;
use webrtc::peer_connection::configuration::RTCConfiguration;
use webrtc::peer_connection::RTCPeerConnection;
use webrtc::rtp_transceiver::rtp_codec::{
    RTCRtpCodecCapability, RTCRtpCodecParameters, RTPCodecType,
};

use crate::errors::AudioError;

/// MIME type we register on the MediaEngine. The webrtc-rs codec table
/// recognises this string and lets the negotiator route packets to it.
pub const L16_MIME: &str = "audio/L16";

/// Build a webrtc-rs `API` instance with our codec registry and default
/// interceptors (jitter buffer, NACK, RTCP reports).
pub fn build_api() -> Result<API, AudioError> {
    let mut media = MediaEngine::default();
    media
        .register_codec(
            RTCRtpCodecParameters {
                capability: RTCRtpCodecCapability {
                    mime_type: L16_MIME.to_string(),
                    clock_rate: 48_000,
                    channels: 1,
                    sdp_fmtp_line: String::new(),
                    rtcp_feedback: vec![],
                },
                payload_type: 96, // dynamic PT range
                ..Default::default()
            },
            RTPCodecType::Audio,
        )
        .map_err(|e| AudioError::WebRtc(format!("register L16 codec: {e}")))?;

    let mut registry = Registry::new();
    registry = register_default_interceptors(registry, &mut media)
        .map_err(|e| AudioError::WebRtc(format!("register interceptors: {e}")))?;

    Ok(APIBuilder::new()
        .with_media_engine(media)
        .with_interceptor_registry(registry)
        .build())
}

/// LAN-only configuration — no STUN, no TURN. We rely entirely on host
/// candidates, which on a paired LAN means the WebRTC `ice_servers` field
/// stays empty.
pub fn lan_only_config() -> RTCConfiguration {
    RTCConfiguration {
        ice_servers: vec![],
        ..Default::default()
    }
}

/// Construct a new PeerConnection ready for offer/answer.
pub async fn new_peer_connection(api: &API) -> Result<Arc<RTCPeerConnection>, AudioError> {
    api.new_peer_connection(lan_only_config())
        .await
        .map(Arc::new)
        .map_err(|e| AudioError::WebRtc(format!("new_peer_connection: {e}")))
}
