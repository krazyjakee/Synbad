//! WebRTC / codec helpers for the audio bridge.
//!
//! Two narrow jobs:
//!
//! 1. Build a [`str0m::Rtc`] with the right knobs flipped for a
//!    LAN-only, sans-I/O audio session. Defaults are fine — we just
//!    pin them in one place so the test code and the runtime code
//!    can't drift.
//!
//! 2. Wrap libopus encode/decode with the framing constants the rest
//!    of the bridge uses (48 kHz mono, 20 ms = 960 samples per
//!    frame). The cpal capture/playback paths already produce
//!    matching `i16` PCM frames; this module is the bytes-on-the-wire
//!    layer between those frames and str0m's Writer/MediaData.

use opus::{Application, Channels, Decoder as OpusDecoder, Encoder as OpusEncoder};
use str0m::Rtc;

use crate::errors::AudioError;

/// 48 kHz mono — the rate cpal capture/playback both target via the
/// resampler in `capture.rs`/`playback.rs`. Opus encodes natively at
/// this rate without any further conversion.
pub const SAMPLE_RATE_HZ: u32 = 48_000;

/// Mono. WebRTC sessions are bidirectional but each direction is a
/// single mono mic stream; stereo would double bandwidth for no gain.
pub const CHANNELS: u16 = 1;

/// One Opus frame = 20 ms of audio. At 48 kHz mono that's 960 samples
/// per frame, matching `capture::FRAME_SAMPLES`. We pick 20 ms because
/// it's libopus's "sweet spot" — smaller frames lose compression
/// efficiency, larger frames add audible latency.
pub const FRAME_SAMPLES: usize = 960;

/// Target Opus bitrate. 64 kbps is comfortable on LAN and well below
/// the 510 kbps libopus ceiling. Higher rates buy diminishing returns
/// for mono speech; lower rates start to thin out at ~32 kbps.
pub const OPUS_BITRATE_BPS: i32 = 64_000;

/// Build a [`str0m::Rtc`] instance with our defaults applied.
///
/// `now` is passed in rather than `Instant::now()`'d here so tests
/// that want deterministic timing (or that need both peers in a pair
/// to share a clock) can drive it themselves.
///
/// Returns a bare `Rtc` — adding local candidates and the SDP
/// `add_media` call are the caller's job because they depend on
/// session role / network setup.
pub fn build_rtc(now: std::time::Instant) -> Rtc {
    // The only config knob we override today is the default crypto
    // provider, which is implicit through the `aws-lc-rs` Cargo
    // feature in `Cargo.toml`. Future tweaks (ICE-lite, transport
    // stats interval, custom certificate) land here.
    Rtc::new(now)
}

/// Make an Opus encoder configured for 48 kHz mono speech-grade
/// audio. Returns the encoder ready to ingest 20 ms PCM frames.
pub fn build_opus_encoder() -> Result<OpusEncoder, AudioError> {
    let mut encoder = OpusEncoder::new(SAMPLE_RATE_HZ, Channels::Mono, Application::Voip)
        .map_err(|e| AudioError::Codec(format!("opus encoder init: {e}")))?;
    encoder
        .set_bitrate(opus::Bitrate::Bits(OPUS_BITRATE_BPS))
        .map_err(|e| AudioError::Codec(format!("opus encoder set_bitrate: {e}")))?;
    Ok(encoder)
}

/// Make an Opus decoder matching [`build_opus_encoder`].
pub fn build_opus_decoder() -> Result<OpusDecoder, AudioError> {
    OpusDecoder::new(SAMPLE_RATE_HZ, Channels::Mono)
        .map_err(|e| AudioError::Codec(format!("opus decoder init: {e}")))
}

/// Decode bound: an Opus packet at 48 kHz mono decompresses to at
/// most 120 ms of audio = 5760 samples. We allocate that much per
/// `decode` call and truncate to what libopus actually wrote.
pub const OPUS_MAX_DECODE_SAMPLES: usize = 5760;
