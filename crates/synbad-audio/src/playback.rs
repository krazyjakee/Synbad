//! Audio playback: incoming `i16` PCM frames → cpal output device.
//!
//! Symmetric to [`capture`](super::capture). Frames arriving over the
//! network are pushed into a SPSC ring buffer; the cpal output callback
//! drains it. Underruns are filled with silence rather than blocking.

#![allow(deprecated)] // cpal 0.17 `DeviceTrait::name`; see devices.rs.

use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{Device, SampleFormat, Stream, StreamConfig};
use tokio::sync::mpsc;
use tracing::{debug, warn};

use crate::capture::TARGET_RATE_HZ;
use crate::errors::AudioError;

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

    let buffer: Arc<Mutex<Vec<i16>>> = Arc::new(Mutex::new(Vec::with_capacity(48_000)));
    let (tx, mut rx) = mpsc::channel::<Arc<[i16]>>(32);

    // Pump task: shuttles incoming frames into the shared buffer.
    let pump_buffer = Arc::clone(&buffer);
    tokio::spawn(async move {
        while let Some(frame) = rx.recv().await {
            let mut buf = pump_buffer.lock().expect("playback buffer poisoned");
            // Cap buffer at 1 second of audio to bound latency on a
            // misbehaving network.
            if buf.len() > TARGET_RATE_HZ as usize {
                buf.clear();
            }
            buf.extend_from_slice(&frame);
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

    // Give cpal a moment to settle before returning so the first frames
    // we push don't get dropped by an still-initializing backend.
    std::thread::sleep(Duration::from_millis(10));
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
                let mut buf_iter = buf.drain(..).peekable();
                for frame in out.chunks_mut(ch) {
                    let sample = buf_iter.next().unwrap_or(0);
                    for slot in frame.iter_mut() {
                        *slot = S::from_mono_i16(sample);
                    }
                }
            },
            err_fn,
            None,
        )
        .map_err(|e| AudioError::StreamBuild(e.to_string()))?;
    Ok(stream)
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
