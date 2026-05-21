//! Audio playback: incoming `i16` PCM frames → cpal output device.
//!
//! Symmetric to [`capture`](super::capture). Frames arriving over the
//! network are pushed into a SPSC ring buffer; the cpal output callback
//! drains it. Underruns are filled with silence rather than blocking.
//! When the output device's native rate isn't 48 kHz, the pump task
//! resamples each frame with [`rubato`] before pushing it to the buffer
//! so the cpal callback never has to leave the real-time-safe path.

#![allow(deprecated)] // cpal 0.17 `DeviceTrait::name`; see devices.rs.

use std::sync::Arc;
use std::sync::Mutex;

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{Device, SampleFormat, Stream, StreamConfig};
use rubato::audioadapter_buffers::direct::SequentialSliceOfVecs;
use rubato::{Fft, FixedSync, Resampler};
use tokio::sync::mpsc;
use tracing::{debug, trace, warn};

use crate::capture::{FRAME_SAMPLES, TARGET_RATE_HZ};
use crate::errors::AudioError;

/// Maximum tolerated jitter buffer depth, in seconds. Anything beyond this is
/// dropped on the pump side to keep latency bounded — at 200 ms a listener can
/// tell something glitched but the bridge stays responsive instead of building
/// up unbounded delay when the network bursts ahead of playback.
const JITTER_CAP_SECS: f32 = 0.2;

/// Build a playback stream and return the sender used to feed it PCM
/// frames. Drop the [`Stream`] to stop playback.
pub fn start_playback(
    device_name: Option<&str>,
) -> Result<(Stream, mpsc::Sender<crate::capture::PcmFrame>), AudioError> {
    let host = cpal::default_host();
    let device = pick_output_device(&host, device_name)?;
    let supported = device
        .default_output_config()
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
        "playback: opening cpal output"
    );

    let mut resampler = PlaybackResampler::new(native_rate)?;
    let buffer: Arc<Mutex<Vec<i16>>> =
        Arc::new(Mutex::new(Vec::with_capacity(native_rate as usize)));
    let (tx, mut rx) = mpsc::channel::<Arc<[i16]>>(32);

    // Pump task: resamples (if needed) and shuttles incoming frames into
    // the shared buffer. When the buffer would exceed the jitter cap, drop
    // the oldest samples rather than the entire buffer so the audible gap
    // stays as short as possible.
    let pump_buffer = Arc::clone(&buffer);
    let jitter_cap = ((native_rate as f32) * JITTER_CAP_SECS) as usize;
    tokio::spawn(async move {
        while let Some(frame) = rx.recv().await {
            let samples = match resampler.as_mut() {
                Some(rs) => rs.process(&frame),
                None => frame.to_vec(),
            };
            if samples.is_empty() {
                continue;
            }
            let mut buf = pump_buffer.lock().expect("playback buffer poisoned");
            let projected = buf.len() + samples.len();
            if projected > jitter_cap {
                let drop_n = projected - jitter_cap / 2;
                let drop_n = drop_n.min(buf.len());
                if drop_n > 0 {
                    buf.drain(..drop_n);
                    trace!(dropped = drop_n, "playback jitter cap exceeded");
                }
            }
            buf.extend_from_slice(&samples);
        }
    });

    let stream = match format {
        SampleFormat::I16 => build_output::<i16>(&device, &cfg, channels, buffer)?,
        SampleFormat::F32 => build_output::<f32>(&device, &cfg, channels, buffer)?,
        other => {
            return Err(AudioError::StreamBuild(format!(
                "unsupported sample format: {other:?}"
            )))
        }
    };
    stream
        .play()
        .map_err(|e| AudioError::StreamBuild(e.to_string()))?;
    Ok((stream, tx))
}

fn pick_output_device(host: &cpal::Host, requested: Option<&str>) -> Result<Device, AudioError> {
    match requested {
        None => host
            .default_output_device()
            .ok_or(AudioError::OutputDeviceNotFound { requested: None }),
        Some(name) => host
            .output_devices()
            .map_err(|e| AudioError::Device(e.to_string()))?
            .find(|d| d.name().map(|n| n == name).unwrap_or(false))
            .ok_or_else(|| AudioError::OutputDeviceNotFound {
                requested: Some(name.to_string()),
            }),
    }
}

fn build_output<S>(
    device: &Device,
    cfg: &StreamConfig,
    channels: u16,
    buffer: Arc<Mutex<Vec<i16>>>,
) -> Result<Stream, AudioError>
where
    S: cpal::SizedSample + FromMonoI16 + cpal::Sample + 'static,
{
    let ch = channels.max(1) as usize;
    let err_fn = |e| warn!(error = ?e, "playback: cpal stream error");
    let stream = device
        .build_output_stream(
            cfg,
            move |out: &mut [S], _info| {
                let mut buf = buffer.lock().expect("playback buffer poisoned");
                fill_output_from_buffer::<S>(out, &mut buf, ch);
            },
            err_fn,
            None,
        )
        .map_err(|e| AudioError::StreamBuild(e.to_string()))?;
    Ok(stream)
}

/// Pop the next `out.len() / ch` mono samples out of `buf` and fan them out
/// to `out` as multi-channel frames, filling any deficit with silence.
///
/// Crucially, this drains only the consumed range — `Vec::drain(..)` discards
/// every element the iterator hasn't yielded by the time it drops, which
/// would lose ~75 % of audio when the cpal callback is smaller than the
/// jitter buffer it's reading from.
fn fill_output_from_buffer<S: FromMonoI16>(out: &mut [S], buf: &mut Vec<i16>, ch: usize) {
    let ch = ch.max(1);
    let want = out.len() / ch;
    let take = want.min(buf.len());
    let mut buf_iter = buf.drain(..take);
    for frame in out.chunks_mut(ch) {
        let sample = buf_iter.next().unwrap_or(0);
        for slot in frame.iter_mut() {
            *slot = S::from_mono_i16(sample);
        }
    }
}

/// Resamples 48 kHz mono `i16` frames down (or up) to the output device's
/// native rate. Held by the pump task; never touched from the cpal callback.
struct PlaybackResampler {
    inner: Fft<f32>,
    /// Mono `f32` samples at 48 kHz awaiting resampling.
    pending_48k: Vec<f32>,
}

impl PlaybackResampler {
    /// Returns `None` when no resampling is required (device runs at
    /// 48 kHz) so the no-resample fast path stays a simple `Vec` copy.
    fn new(native_rate: u32) -> Result<Option<Self>, AudioError> {
        if native_rate == TARGET_RATE_HZ {
            return Ok(None);
        }
        let inner = Fft::<f32>::new(
            TARGET_RATE_HZ as usize,
            native_rate as usize,
            FRAME_SAMPLES,
            1,
            1,
            FixedSync::Input,
        )
        .map_err(|e| AudioError::StreamBuild(format!("playback resampler init: {e}")))?;
        Ok(Some(Self {
            inner,
            pending_48k: Vec::with_capacity(FRAME_SAMPLES * 2),
        }))
    }

    fn process(&mut self, frame: &[i16]) -> Vec<i16> {
        for &s in frame {
            self.pending_48k.push(s as f32 / i16::MAX as f32);
        }
        let mut out_samples = Vec::new();
        loop {
            let needed = self.inner.input_frames_next();
            if self.pending_48k.len() < needed {
                break;
            }
            let input: Vec<f32> = self.pending_48k.drain(..needed).collect();
            let buf: [Vec<f32>; 1] = [input];
            let adapter = match SequentialSliceOfVecs::new(&buf[..], 1, needed) {
                Ok(a) => a,
                Err(e) => {
                    warn!(?e, "playback resampler adapter build failed");
                    continue;
                }
            };
            match self.inner.process(&adapter, 0, None) {
                Ok(out) => {
                    for s in out.take_data() {
                        let clamped = s.clamp(-1.0, 1.0);
                        out_samples.push((clamped * i16::MAX as f32) as i16);
                    }
                }
                Err(e) => warn!(?e, "playback resampler process failed"),
            }
        }
        out_samples
    }
}

trait FromMonoI16: Copy {
    fn from_mono_i16(sample: i16) -> Self;
}

impl FromMonoI16 for i16 {
    fn from_mono_i16(sample: i16) -> Self {
        sample
    }
}

impl FromMonoI16 for f32 {
    fn from_mono_i16(sample: i16) -> Self {
        (sample as f32) / (i16::MAX as f32)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_resampler_at_48k() {
        let rs = PlaybackResampler::new(48_000).expect("build");
        assert!(rs.is_none());
    }

    #[test]
    fn resampler_48k_to_44k1_produces_native_rate_output() {
        let mut rs = PlaybackResampler::new(44_100)
            .expect("build")
            .expect("non-48k device should produce a resampler");
        // Feed 1 s = 50 frames; expect ~44100 output samples.
        let frame: Vec<i16> = vec![1234; FRAME_SAMPLES];
        let mut produced = 0;
        for _ in 0..50 {
            produced += rs.process(&frame).len();
        }
        // Allow modest tolerance for the FFT resampler's startup delay.
        assert!(
            (43_000..=44_200).contains(&produced),
            "expected ~44100 samples, got {produced}"
        );
    }

    #[test]
    fn fill_output_consumes_only_what_it_needs() {
        // Regression test for the drain-everything bug: a callback smaller
        // than the buffered audio must leave the tail in the buffer.
        let mut buf: Vec<i16> = (0..2000i16).collect();
        let mut out = vec![0i16; 480]; // mono callback, 480 samples
        fill_output_from_buffer::<i16>(&mut out, &mut buf, 1);

        assert_eq!(out.first(), Some(&0));
        assert_eq!(out.last(), Some(&479));
        assert_eq!(buf.len(), 2000 - 480, "tail must stay in the buffer");
        assert_eq!(buf.first(), Some(&480), "next callback resumes here");
    }

    #[test]
    fn fill_output_fans_mono_to_stereo() {
        let mut buf: Vec<i16> = vec![1000, 2000, 3000];
        let mut out = vec![0i16; 6]; // 3 frames * 2 channels
        fill_output_from_buffer::<i16>(&mut out, &mut buf, 2);
        assert_eq!(out, vec![1000, 1000, 2000, 2000, 3000, 3000]);
        assert!(buf.is_empty());
    }

    #[test]
    fn fill_output_pads_underrun_with_silence() {
        let mut buf: Vec<i16> = vec![7777];
        let mut out = vec![123i16; 4]; // stereo, 2 frames; only one sample available
        fill_output_from_buffer::<i16>(&mut out, &mut buf, 2);
        assert_eq!(out, vec![7777, 7777, 0, 0]);
        assert!(buf.is_empty());
    }

    #[test]
    fn resampler_48k_to_96k_doubles_samples() {
        let mut rs = PlaybackResampler::new(96_000)
            .expect("build")
            .expect("non-48k device should produce a resampler");
        let frame: Vec<i16> = vec![5678; FRAME_SAMPLES];
        let mut produced = 0;
        for _ in 0..10 {
            produced += rs.process(&frame).len();
        }
        // 10 frames * 960 * (96000/48000) = 19_200 samples expected.
        assert!(
            (18_000..=19_400).contains(&produced),
            "expected ~19200 samples, got {produced}"
        );
    }
}
