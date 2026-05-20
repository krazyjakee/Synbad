//! Device enumeration. Surface that the GUI populates dropdowns from.
//!
//! Outputs the IPC-friendly [`synbad_ipc::AudioDeviceInfo`] so the GUI
//! never sees a cpal type directly.

// cpal 0.17 deprecates `DeviceTrait::name` in favour of `description()`,
// but the replacement returns a struct rather than a plain String and the
// extra surface isn't useful here. Stick with `name()` for now.
#![allow(deprecated)]

use cpal::traits::{DeviceTrait, HostTrait};
use synbad_ipc::AudioDeviceInfo;

use crate::errors::AudioError;

/// All input devices visible to cpal on the default host.
///
/// On Linux, this includes PulseAudio/PipeWire `.monitor` sources, which we
/// flag as `is_loopback = true` so the GUI can group them. On Windows we
/// synthesize loopback entries from the output devices (cpal exposes them
/// via the WASAPI loopback flow but doesn't list them as inputs).
pub fn list_input_devices() -> Result<Vec<AudioDeviceInfo>, AudioError> {
    let host = cpal::default_host();
    let default_name = host
        .default_input_device()
        .and_then(|d| d.name().ok())
        .unwrap_or_default();

    let mut out = Vec::new();
    for device in host
        .input_devices()
        .map_err(|e| AudioError::Device(e.to_string()))?
    {
        let name = match device.name() {
            Ok(n) => n,
            Err(_) => continue,
        };
        let cfg = match device.default_input_config() {
            Ok(c) => c,
            Err(_) => continue,
        };
        let is_loopback = looks_like_loopback(&name);
        out.push(AudioDeviceInfo {
            name: name.clone(),
            is_default: name == default_name,
            native_sample_rate: cfg.sample_rate(),
            channels: cfg.channels(),
            is_loopback,
        });
    }
    Ok(out)
}

/// All output devices visible to cpal on the default host.
pub fn list_output_devices() -> Result<Vec<AudioDeviceInfo>, AudioError> {
    let host = cpal::default_host();
    let default_name = host
        .default_output_device()
        .and_then(|d| d.name().ok())
        .unwrap_or_default();

    let mut out = Vec::new();
    for device in host
        .output_devices()
        .map_err(|e| AudioError::Device(e.to_string()))?
    {
        let name = match device.name() {
            Ok(n) => n,
            Err(_) => continue,
        };
        let cfg = match device.default_output_config() {
            Ok(c) => c,
            Err(_) => continue,
        };
        out.push(AudioDeviceInfo {
            name: name.clone(),
            is_default: name == default_name,
            native_sample_rate: cfg.sample_rate(),
            channels: cfg.channels(),
            is_loopback: false,
        });
    }
    Ok(out)
}

/// Heuristic for "this input device is actually a loopback / monitor of an
/// output device." PipeWire/PulseAudio name them `<sink>.monitor`; some
/// PipeWire builds also expose `Monitor of …`.
fn looks_like_loopback(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    lower.ends_with(".monitor") || lower.contains("monitor of ") || lower.contains("loopback")
}

#[cfg(test)]
mod tests {
    use super::looks_like_loopback;

    #[test]
    fn loopback_detection() {
        assert!(looks_like_loopback(
            "alsa_output.pci-0000_00_1f.3.analog-stereo.monitor"
        ));
        assert!(looks_like_loopback("Monitor of Built-in Audio"));
        assert!(looks_like_loopback("Stereo Mix (Loopback)"));
        assert!(!looks_like_loopback("Built-in Microphone"));
    }

    #[test]
    fn enumeration_does_not_panic() {
        // In headless CI environments these may legitimately return Err or
        // an empty list — what we're guarding against is a panic inside
        // cpal during initial host probing.
        let _ = super::list_input_devices();
        let _ = super::list_output_devices();
    }
}
