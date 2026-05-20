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
use rubato::audioadapter_buffers::direct::SequentialSliceOfVecs;
use rubato::{Fft, FixedSync, Resampler};
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

    let accumulator = MonoAccumulator::new(channels, native_rate)?;
    let (frame_tx, frame_rx) = mpsc::channel::<Arc<[i16]>>(32);
    let stream = match format {
        SampleFormat::I16 => build_input::<i16>(&device, &cfg, frame_tx, accumulator)?,
        SampleFormat::F32 => build_input::<f32>(&device, &cfg, frame_tx, accumulator)?,
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
    let result = match requested {
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
    };
    // On macOS, loopback ("client speakers → server") requires a virtual
    // audio device. If we couldn't satisfy the request and none of the
    // visible input devices look loopback-capable, surface a specific
    // error so the GUI can link to docs/AUDIO.md (BlackHole install) —
    // way more useful than a generic "not found".
    match result {
        Err(AudioError::InputDeviceNotFound { .. }) if !any_loopback_input_available(host) => {
            Err(AudioError::LoopbackUnavailable)
        }
        other => other,
    }
}

#[cfg(target_os = "macos")]
fn any_loopback_input_available(host: &cpal::Host) -> bool {
    host.input_devices()
        .ok()
        .into_iter()
        .flatten()
        .filter_map(|d| d.name().ok())
        .any(|n| crate::devices::looks_like_loopback(&n))
}

#[cfg(not(target_os = "macos"))]
fn any_loopback_input_available(_host: &cpal::Host) -> bool {
    // Linux exposes `.monitor` sources by default; Windows exposes
    // loopback through WASAPI without a virtual device. Treat the
    // platform as inherently capable so we don't paper over a real
    // device-not-found with a misleading "install BlackHole" hint.
    true
}

fn build_input<S>(
    device: &Device,
    cfg: &StreamConfig,
    frame_tx: mpsc::Sender<PcmFrame>,
    mut accumulator: MonoAccumulator,
) -> Result<Stream, AudioError>
where
    S: cpal::SizedSample + ToMonoF32 + 'static,
{
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

/// Downmixes to mono, resamples to 48 kHz when needed, and emits
/// fixed-size 20 ms `i16` frames.
struct MonoAccumulator {
    channels: u16,
    /// 48 kHz mono `i16` samples waiting to be chunked into 960-sample frames.
    pending_48k: Vec<i16>,
    /// `None` when the device already runs at 48 kHz; the no-resample path
    /// keeps the cost down for what is by far the common case.
    resampler: Option<ResamplerState>,
}

struct ResamplerState {
    inner: Fft<f32>,
    /// Mono `f32` samples at the device's native rate awaiting resampling.
    pending_native: Vec<f32>,
}

impl MonoAccumulator {
    fn new(channels: u16, native_rate: u32) -> Result<Self, AudioError> {
        let resampler = if native_rate == TARGET_RATE_HZ {
            None
        } else {
            // Aim for ~20 ms of input per resampler call. rubato may round
            // the actual chunk size up to a multiple of its internal FFT
            // subdivision, so we always re-query `input_frames_next()`
            // before feeding instead of caching.
            let chunk_hint = (native_rate as usize).div_ceil(50);
            let inner = Fft::<f32>::new(
                native_rate as usize,
                TARGET_RATE_HZ as usize,
                chunk_hint,
                1,
                1,
                FixedSync::Input,
            )
            .map_err(|e| AudioError::StreamBuild(format!("resampler init: {e}")))?;
            Some(ResamplerState {
                inner,
                pending_native: Vec::with_capacity(chunk_hint * 2),
            })
        };
        Ok(Self {
            channels,
            pending_48k: Vec::with_capacity(FRAME_SAMPLES * 2),
            resampler,
        })
    }

    fn feed<S: ToMonoF32>(&mut self, data: &[S]) -> Vec<PcmFrame> {
        let ch = self.channels.max(1) as usize;
        let Self {
            pending_48k,
            resampler,
            ..
        } = self;

        if let Some(rs) = resampler.as_mut() {
            for chunk in data.chunks_exact(ch) {
                rs.pending_native.push(S::mono_f32(chunk));
            }
            loop {
                let needed = rs.inner.input_frames_next();
                if rs.pending_native.len() < needed {
                    break;
                }
                let input: Vec<f32> = rs.pending_native.drain(..needed).collect();
                let buf: [Vec<f32>; 1] = [input];
                let adapter = match SequentialSliceOfVecs::new(&buf[..], 1, needed) {
                    Ok(a) => a,
                    Err(e) => {
                        warn!(?e, "resampler input adapter build failed");
                        continue;
                    }
                };
                match rs.inner.process(&adapter, 0, None) {
                    Ok(out) => {
                        for s in out.take_data() {
                            pending_48k.push(f32_to_i16(s));
                        }
                    }
                    Err(e) => {
                        warn!(?e, "resampler process failed; dropping chunk");
                    }
                }
            }
        } else {
            for chunk in data.chunks_exact(ch) {
                pending_48k.push(f32_to_i16(S::mono_f32(chunk)));
            }
        }

        let mut out = Vec::new();
        while pending_48k.len() >= FRAME_SAMPLES {
            let tail = pending_48k.split_off(FRAME_SAMPLES);
            let frame = std::mem::replace(pending_48k, tail);
            out.push(PcmFrame::from(frame));
        }
        out
    }
}

/// Convert one cpal sample chunk (one frame's worth of channels) to a
/// single mono `f32` in roughly the `[-1.0, 1.0]` range.
trait ToMonoF32: Copy {
    fn mono_f32(channels: &[Self]) -> f32;
}

impl ToMonoF32 for i16 {
    fn mono_f32(channels: &[Self]) -> f32 {
        let sum: i32 = channels.iter().map(|s| *s as i32).sum();
        let avg = sum / channels.len() as i32;
        avg as f32 / i16::MAX as f32
    }
}

impl ToMonoF32 for f32 {
    fn mono_f32(channels: &[Self]) -> f32 {
        channels.iter().sum::<f32>() / channels.len() as f32
    }
}

fn f32_to_i16(sample: f32) -> i16 {
    let clamped = sample.clamp(-1.0, 1.0);
    (clamped * i16::MAX as f32) as i16
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mono_accumulator_at_native_48k_emits_full_frames() {
        let mut acc = MonoAccumulator::new(1, 48_000).expect("build accumulator");
        let frames = acc.feed::<i16>(&vec![1234i16; 2000]);
        assert_eq!(frames.len(), 2, "2000 samples → two 960-sample frames");
        assert_eq!(frames[0].len(), FRAME_SAMPLES);
        // Remainder (2000 - 1920 = 80 samples) is buffered for the next call.
        let more = acc.feed::<i16>(&vec![1234i16; 880]);
        assert_eq!(more.len(), 1);
    }

    #[test]
    fn mono_accumulator_resamples_44k1_up_to_48k() {
        // 1 second of audio at 44.1 kHz should produce ~50 frames at 48 kHz.
        let mut acc = MonoAccumulator::new(1, 44_100).expect("build accumulator");
        let input = vec![0.25f32; 44_100];
        let mut emitted = 0;
        // Feed in cpal-callback-sized chunks so we exercise the loop.
        for chunk in input.chunks(441) {
            emitted += acc.feed::<f32>(chunk).len();
        }
        // Allow for resampler delay swallowing the first frame or two; we
        // primarily care that the silent-drop bug is gone.
        assert!(
            emitted >= 45,
            "expected ~50 frames out of 1 s @ 44.1k, got {emitted}"
        );
    }

    #[test]
    fn mono_accumulator_downsamples_96k_to_48k() {
        let mut acc = MonoAccumulator::new(2, 96_000).expect("build accumulator");
        // 1 s stereo at 96 kHz = 192_000 interleaved samples.
        let input = vec![0.1f32; 192_000];
        let mut emitted = 0;
        for chunk in input.chunks(1920) {
            emitted += acc.feed::<f32>(chunk).len();
        }
        assert!(
            emitted >= 45,
            "expected ~50 frames out of 1 s @ 96k, got {emitted}"
        );
    }
}
