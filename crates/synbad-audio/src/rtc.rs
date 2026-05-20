//! WebRTC PeerConnection setup for audio bridging.
//!
//! Wires up:
//! - A `MediaEngine` that advertises raw L16 mono 48 kHz audio. webrtc-rs
//!   0.17 doesn't ship an L16 codec preset (its [`payloader_for_codec`]
//!   lookup table covers only Opus / G7xx / VP8 / VP9 / H264 / H265 /
//!   AV1) so we register a custom [`RTCRtpCodecParameters`] and bypass
//!   the [`TrackLocalStaticSample`] path entirely.
//! - A single audio-only [`TrackLocalStaticRTP`] in `sendrecv` mode.
//!   We hand-roll RFC 3551 §4.5.7 L16 packetisation: one 20 ms frame of
//!   960 mono samples per RTP packet, payload type 96, big-endian on the
//!   wire.
//! - An empty `ICEServers` list — we're LAN-only and rely on host
//!   candidates the OS already advertises.
//!
//! [`payloader_for_codec`]: webrtc::rtp_transceiver::rtp_codec::RTCRtpCodecCapability::payloader_for_codec
//! [`TrackLocalStaticSample`]: webrtc::track::track_local::track_local_static_sample::TrackLocalStaticSample

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
use webrtc::track::track_local::track_local_static_rtp::TrackLocalStaticRTP;

use crate::errors::AudioError;

/// MIME type we register on the MediaEngine. The webrtc-rs codec table
/// recognises this string and lets the negotiator route packets to it.
pub const L16_MIME: &str = "audio/L16";

/// Dynamic RTP payload type for our L16 codec. 96 is the lowest value in
/// the dynamic range and is conventional for custom payloads.
pub const L16_PAYLOAD_TYPE: u8 = 96;

/// L16 RTP clock rate in Hz. RFC 3551 §4.5.7 defines L16 as a generic
/// PCM-16 payload at any sample rate; 48 kHz is what we capture/play
/// throughout the bridge.
pub const L16_CLOCK_RATE: u32 = 48_000;

/// Samples per 20 ms RTP frame at 48 kHz mono. Matches
/// [`crate::capture::FRAME_SAMPLES`].
pub const SAMPLES_PER_PACKET: u32 = 960;

fn l16_codec_capability() -> RTCRtpCodecCapability {
    RTCRtpCodecCapability {
        mime_type: L16_MIME.to_string(),
        clock_rate: L16_CLOCK_RATE,
        channels: 1,
        sdp_fmtp_line: String::new(),
        rtcp_feedback: vec![],
    }
}

/// Build a webrtc-rs `API` instance with our codec registry and default
/// interceptors (jitter buffer, NACK, RTCP reports).
pub fn build_api() -> Result<API, AudioError> {
    let mut media = MediaEngine::default();
    media
        .register_codec(
            RTCRtpCodecParameters {
                capability: l16_codec_capability(),
                payload_type: L16_PAYLOAD_TYPE,
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

/// Build the outbound L16 audio track. Stream-id is reused for both
/// directions of a paired session — webrtc-rs uses it only to group
/// tracks, and we only ever send one audio track per session.
pub fn build_outbound_track() -> Arc<TrackLocalStaticRTP> {
    Arc::new(TrackLocalStaticRTP::new(
        l16_codec_capability(),
        "audio".to_string(),
        "synbad-audio".to_string(),
    ))
}
