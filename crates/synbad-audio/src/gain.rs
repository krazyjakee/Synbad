//! Live-updatable linear gain shared between the bridge and the
//! capture/playback streams.
//!
//! cpal callbacks run on a real-time audio thread that mustn't take a
//! Mutex, so the gain value is held in an [`AtomicU32`] (storing the f32
//! bit pattern). The bridge writes new values when the user moves a
//! slider; the audio threads read with `Relaxed` ordering on every
//! sample batch. There's no synchronization requirement between gain
//! updates and the audio they affect — slight delay is fine.

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;

/// Shared handle to a linear gain multiplier. Cheap to `clone` — it's
/// `Arc` under the hood.
#[derive(Debug, Clone)]
pub struct GainHandle {
    bits: Arc<AtomicU32>,
}

impl GainHandle {
    pub fn new(initial: f32) -> Self {
        Self {
            bits: Arc::new(AtomicU32::new(initial.to_bits())),
        }
    }

    /// Current linear gain. Loads with `Relaxed`; the audio thread
    /// doesn't need to synchronize with anything else on the producer
    /// side.
    #[inline]
    pub fn get(&self) -> f32 {
        f32::from_bits(self.bits.load(Ordering::Relaxed))
    }

    /// Replace the stored gain. Called from the bridge when the user
    /// edits a slider in the GUI.
    pub fn set(&self, value: f32) {
        self.bits.store(value.to_bits(), Ordering::Relaxed);
    }
}

impl Default for GainHandle {
    fn default() -> Self {
        Self::new(1.0)
    }
}

/// Apply a linear gain to an `i16` sample with saturating clamp at the
/// i16 range. Used by both capture (post-mono-conversion) and playback
/// (pre-write-to-cpal) so values above 1.0 don't wrap.
#[inline]
pub fn apply_gain_i16(sample: i16, gain: f32) -> i16 {
    if gain == 1.0 {
        return sample;
    }
    let scaled = sample as f32 * gain;
    if scaled >= i16::MAX as f32 {
        i16::MAX
    } else if scaled <= i16::MIN as f32 {
        i16::MIN
    } else {
        scaled as i16
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unity_gain_is_identity() {
        assert_eq!(apply_gain_i16(1234, 1.0), 1234);
        assert_eq!(apply_gain_i16(-1234, 1.0), -1234);
    }

    #[test]
    fn zero_gain_mutes() {
        assert_eq!(apply_gain_i16(12345, 0.0), 0);
        assert_eq!(apply_gain_i16(-32000, 0.0), 0);
    }

    #[test]
    fn boost_saturates_at_i16_bounds() {
        assert_eq!(apply_gain_i16(20_000, 2.0), i16::MAX);
        assert_eq!(apply_gain_i16(-20_000, 2.0), i16::MIN);
    }

    #[test]
    fn fractional_gain_attenuates() {
        assert_eq!(apply_gain_i16(10_000, 0.5), 5_000);
    }

    #[test]
    fn handle_round_trips() {
        let h = GainHandle::new(0.75);
        assert_eq!(h.get(), 0.75);
        h.set(1.5);
        assert_eq!(h.get(), 1.5);
    }
}
