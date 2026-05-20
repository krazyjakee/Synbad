//! Error type surfaced by every public entry point in this crate.

use thiserror::Error;

#[derive(Debug, Error)]
pub enum AudioError {
    #[error("audio device error: {0}")]
    Device(String),

    #[error("no input device matched {requested:?}")]
    InputDeviceNotFound { requested: Option<String> },

    #[error("no output device matched {requested:?}")]
    OutputDeviceNotFound { requested: Option<String> },

    #[error("loopback capture not available on this platform without a virtual audio device (see docs/AUDIO.md)")]
    LoopbackUnavailable,

    #[error("cpal stream build failed: {0}")]
    StreamBuild(String),

    #[error("webrtc error: {0}")]
    WebRtc(String),

    #[error("signaling protocol error: {0}")]
    Signal(String),

    #[error("signaling transport closed")]
    SignalClosed,

    #[error("peer is not in the trust store: {0}")]
    UntrustedPeer(String),

    #[error("io: {0}")]
    Io(#[from] std::io::Error),

    #[error("json: {0}")]
    Json(#[from] serde_json::Error),
}
