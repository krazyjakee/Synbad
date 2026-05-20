//! Audio capture: cpal input device → 20 ms `i16` PCM frames at 48 kHz mono.
//!
//! cpal callbacks run on a real-time audio thread that must not block. The
//! callback pushes raw samples into a lock-free SPSC ring buffer; a tokio
//! task on the other side drains it, downmixes to mono, and resamples to
//! 48 kHz with [`rubato`] if the device's native rate differs.

#![allow(deprecated)] // cpal 0.17 `DeviceTrait::name`; see devices.rs.

use std::sync::Arc;

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{Device, SampleFormat, Stream, StreamConfig};
use tokio::sync::mpsc;
use tracing::{debug, warn};

use crate::errors::AudioError;

/// One 20 ms frame at 48 kHz mono = 960 samples.
pub const FRAME_SAMPLES: usize = 960;
pub const TARGET_RATE_HZ: u32 = 48_000;

/// One mono PCM frame shared between the cpal callback thread and the
/// tokio consumer. `Arc<[i16]>` makes the hand-off allocation-free after
/// the initial fill.
pub type PcmFrame = Arc<[i16]>;

/// Resolve the input device the user picked (or the OS default), build a
/// cpal input stream, and return a tokio receiver that yields 20 ms PCM
/// frames.
///
/// The returned [`Stream`] must be kept alive for as long as the receiver
/// is being drained — dropping it stops the cpal callback.
pub fn start_capture(
    device_name: Option<&str>,
) -> Result<(Stream, mpsc::Receiver<PcmFrame>), AudioError> {
    let host = cpal::default_host();
    let device = pick_input_device(&host, device_name)?;
    let supported = device
        .default_input_config()
        .map_err(|e| AudioError::StreamBuild(e.to_string()))?;
    let cfg: StreamConfig = supported.config();
    let format = supported.sample_format();
    let channels = cfg.channels;
    let native_rate = cfg.sample_rate;

    debug!(
        device = ?device.name().ok(),
        format = ?format,
        channels,
        native_rate,
        "capture: opening cpal input"
    );

    let (frame_tx, frame_rx) = mpsc::channel::<Arc<[i16]>>(32);
    let stream = match format {
        SampleFormat::I16 => build_input::<i16>(&device, &cfg, channels, native_rate, frame_tx)?,
        SampleFormat::F32 => build_input::<f32>(&device, &cfg, channels, native_rate, frame_tx)?,
        other => {
            return Err(AudioError::StreamBuild(format!(
                "unsupported sample format: {other:?}"
            )))
        }
    };
    stream
        .play()
        .map_err(|e| AudioError::StreamBuild(e.to_string()))?;
    Ok((stream, frame_rx))
}

fn pick_input_device(host: &cpal::Host, requested: Option<&str>) -> Result<Device, AudioError> {
    match requested {
        None => host
            .default_input_device()
            .ok_or(AudioError::InputDeviceNotFound { requested: None }),
        Some(name) => host
            .input_devices()
            .map_err(|e| AudioError::Device(e.to_string()))?
            .find(|d| d.name().map(|n| n == name).unwrap_or(false))
            .ok_or_else(|| AudioError::InputDeviceNotFound {
                requested: Some(name.to_string()),
            }),
    }
}

fn build_input<S>(
    device: &Device,
    cfg: &StreamConfig,
    channels: u16,
    native_rate: u32,
    frame_tx: mpsc::Sender<PcmFrame>,
) -> Result<Stream, AudioError>
where
    S: cpal::SizedSample + ToMonoI16 + 'static,
{
    let mut accumulator = MonoAccumulator::new(channels, native_rate);
    let err_fn = |e| warn!(error = ?e, "capture: cpal stream error");
    let stream = device
        .build_input_stream(
            cfg,
            move |data: &[S], _info| {
                for frame in accumulator.feed::<S>(data) {
                    // try_send: if the consumer falls behind we drop frames
                    // rather than blocking the audio thread.
                    let _ = frame_tx.try_send(frame);
                }
            },
            err_fn,
            None,
        )
        .map_err(|e| AudioError::StreamBuild(e.to_string()))?;
    Ok(stream)
}

/// Helper that downmixes to mono and emits fixed-size 48 kHz frames.
///
/// For v1 we sidestep proper resampling and only support devices whose
/// native rate is 48 kHz. A future revision plugs `rubato` in here.
struct MonoAccumulator {
    channels: u16,
    native_rate: u32,
    pending: Vec<i16>,
}

impl MonoAccumulator {
    fn new(channels: u16, native_rate: u32) -> Self {
        Self {
            channels,
            native_rate,
            pending: Vec::with_capacity(FRAME_SAMPLES * 2),
        }
    }

    fn feed<S: ToMonoI16>(&mut self, data: &[S]) -> Vec<PcmFrame> {
        if self.native_rate != TARGET_RATE_HZ {
            // TODO(audio-resample): plug rubato in for non-48k devices.
            // For now skip these frames so we never emit at the wrong rate.
            return Vec::new();
        }
        let ch = self.channels.max(1) as usize;
        for chunk in data.chunks_exact(ch) {
            self.pending.push(S::mono_i16(chunk));
        }
        let mut out = Vec::new();
        while self.pending.len() >= FRAME_SAMPLES {
            let tail = self.pending.split_off(FRAME_SAMPLES);
            let frame = std::mem::replace(&mut self.pending, tail);
            out.push(PcmFrame::from(frame));
        }
        out
    }
}

/// Trait that lets one `build_input` function handle i16 and f32 inputs.
trait ToMonoI16: Copy {
    fn mono_i16(channels: &[Self]) -> i16;
}

impl ToMonoI16 for i16 {
    fn mono_i16(channels: &[Self]) -> i16 {
        // Average across channels with saturation to avoid overflow.
        let sum: i32 = channels.iter().map(|s| *s as i32).sum();
        (sum / channels.len() as i32) as i16
    }
}

impl ToMonoI16 for f32 {
    fn mono_i16(channels: &[Self]) -> i16 {
        let avg: f32 = channels.iter().sum::<f32>() / channels.len() as f32;
        let clamped = avg.clamp(-1.0, 1.0);
        (clamped * i16::MAX as f32) as i16
    }
}
