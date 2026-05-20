//! LAN audio bridge for Synbad.
//!
//! Captures local audio (microphone, system loopback) and transports it to
//! paired peers over a WebRTC PeerConnection; receives peer audio and plays
//! it back through the local output device.
//!
//! ## Architecture
//!
//! ```text
//!   cpal input ──► capture ──► RTP/SRTP ──► webrtc-rs PeerConnection ──► peer
//!                                                                          │
//!   cpal output ◄── playback ◄── RTP/SRTP ◄── webrtc-rs PeerConnection ◄──┘
//! ```
//!
//! Signaling (SDP offer/answer + ICE) runs over an authenticated
//! [`synbad_crypto::CipherStream`] on its own TCP port — the same pattern
//! `synbad-sync` uses for config replication, with its own protocol domain
//! (`b"synbad-audio-v1"`).
//!
//! ## Public entry point
//!
//! [`AudioBridge::new`] constructs the bridge; [`AudioBridge::spawn`]
//! moves it onto a background task and returns an [`AudioBridgeHandle`]
//! the supervisor uses to send commands and drain events.

pub mod bridge;
pub mod capture;
pub mod devices;
pub mod errors;
pub mod playback;
pub mod protocol;
pub mod rtc;
pub mod session;

pub use bridge::{AudioBridge, AudioBridgeHandle, AudioCommand, AudioEvent};
pub use errors::AudioError;
pub use protocol::{AudioSignal, SIGNAL_DOMAIN};
pub use session::SessionRole;
