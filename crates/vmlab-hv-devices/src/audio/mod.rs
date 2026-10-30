//! The host-audio seam.
//!
//! Guest audio devices ([`crate::hda`], later virtio-snd) produce and
//! consume interleaved PCM periods. They never talk to a sound server
//! directly: they push playback periods into a [`PlaybackSink`] and pull
//! capture periods from a [`CaptureSource`], both opened from a
//! [`HostAudio`] backend. Backends live in this module — `null` always,
//! `recording` for tests, `pipewire` and `pulse` behind cargo features —
//! and are chosen at machine start by [`select_backend`] in the order the
//! audio plan fixes: PipeWire when its socket is reachable, else
//! PulseAudio when `PULSE_SERVER` is set, else null with one warning.
//!
//! The contract every backend keeps:
//!
//! - `write_period` and `read_period` never block. The device sits on the
//!   vCPU path; a slow host degrades to dropped or silent periods.
//! - Playback overrun drops the oldest period; capture underrun supplies
//!   silence.
//! - `latency_bytes` reports host-side PCM bytes buffered beyond the
//!   device, so devices can pace guest-visible position against real
//!   playback and report host latency to the guest.

pub mod null;
#[cfg(any(test, feature = "testing"))]
pub mod recording;
#[cfg(any(test, feature = "pipewire", feature = "pulse"))]
pub(crate) mod ring;

#[cfg(feature = "pipewire")]
pub mod pipewire;
#[cfg(feature = "pulse")]
pub mod pulse;

use std::sync::Arc;

use crate::DeviceError;

/// PCM sample encodings the seam carries.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SampleFormat {
    /// Interleaved signed 16-bit little-endian.
    S16Le,
    /// Interleaved IEEE 754 single-precision little-endian.
    F32Le,
}

impl SampleFormat {
    fn bytes_per_sample(self) -> usize {
        match self {
            SampleFormat::S16Le => 2,
            SampleFormat::F32Le => 4,
        }
    }
}

/// One interleaved PCM stream format, as a guest device negotiated it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AudioFormat {
    pub sample_format: SampleFormat,
    /// 1 or 2.
    pub channels: u8,
    /// Sample rate in Hz, e.g. 44100, 48000, 96000.
    pub rate: u32,
}

impl AudioFormat {
    /// Bytes in one frame (all channels of one sample instant).
    pub fn frame_bytes(&self) -> usize {
        self.sample_format.bytes_per_sample() * self.channels as usize
    }
}

/// Where a device pushes playback periods.
pub trait PlaybackSink: Send {
    /// Hand one period of interleaved PCM to the host. `pcm.len()` is a
    /// whole number of frames; the call never blocks.
    fn write_period(&mut self, pcm: &[u8]);
    /// Host-side PCM bytes accepted but not yet audible.
    fn latency_bytes(&self) -> usize;
    /// PCM waiting for the host callback, excluding downstream graph latency.
    fn queued_bytes(&self) -> usize {
        self.latency_bytes()
    }
}

/// Where a device pulls capture periods from.
pub trait CaptureSource: Send {
    /// Fill `out` with one period of interleaved PCM, padding with silence
    /// when the host has nothing new; the call never blocks.
    fn read_period(&mut self, out: &mut [u8]);
    /// Host-side PCM bytes captured but not yet consumed by the device.
    fn latency_bytes(&self) -> usize;
}

/// A host audio backend: opens streams for one machine's device.
pub trait HostAudio: Send + Sync {
    /// Backend name for logs (`"pipewire"`, `"pulse"`, `"null"`, ...).
    fn name(&self) -> &'static str;
    fn open_playback(&self, format: AudioFormat) -> Result<Box<dyn PlaybackSink>, DeviceError>;
    fn open_capture(&self, format: AudioFormat) -> Result<Box<dyn CaptureSource>, DeviceError>;
}

/// The environment facts the selection order runs on. Split out so the
/// order is testable without touching process-global environment
/// variables.
#[derive(Debug, Clone, Copy, Default)]
pub struct HostProbe {
    /// The `pipewire` backend is compiled in.
    pub pipewire_compiled: bool,
    /// PipeWire's socket under `XDG_RUNTIME_DIR` accepts a connection.
    pub pipewire_socket_reachable: bool,
    /// The `pulse` backend is compiled in.
    pub pulse_compiled: bool,
    /// `PULSE_SERVER` is set to a non-empty value.
    pub pulse_server_set: bool,
}

/// Which backend wins, and why the rest lost.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HostChoice {
    PipeWire,
    Pulse,
    Null,
}

impl HostChoice {
    /// Why the losers lost, for the one warning line on stderr.
    pub fn null_reason(&self) -> Option<&'static str> {
        match self {
            HostChoice::PipeWire | HostChoice::Pulse => None,
            HostChoice::Null => Some(
                "no usable compiled host audio backend: PipeWire unavailable or disabled; PulseAudio disabled or PULSE_SERVER unset",
            ),
        }
    }
}

/// The selection order: PipeWire on a reachable socket, then PulseAudio
/// when `PULSE_SERVER` is set, else null.
pub fn choose_host(probe: &HostProbe) -> HostChoice {
    if probe.pipewire_compiled && probe.pipewire_socket_reachable {
        return HostChoice::PipeWire;
    }
    if probe.pulse_compiled && probe.pulse_server_set {
        return HostChoice::Pulse;
    }
    HostChoice::Null
}

/// Probe the process environment the way the spec's selection order
/// describes.
pub fn probe_environment() -> HostProbe {
    HostProbe {
        pipewire_compiled: cfg!(feature = "pipewire"),
        pipewire_socket_reachable: pipewire_socket_reachable(),
        pulse_compiled: cfg!(feature = "pulse"),
        pulse_server_set: std::env::var_os("PULSE_SERVER").is_some_and(|v| !v.is_empty()),
    }
}

/// True when PipeWire's server socket under `XDG_RUNTIME_DIR` accepts a
/// connection.
fn pipewire_socket_reachable() -> bool {
    let Some(runtime_dir) = std::env::var_os("XDG_RUNTIME_DIR") else {
        return false;
    };
    let socket = std::path::Path::new(&runtime_dir).join("pipewire-0");
    std::os::unix::net::UnixStream::connect(&socket).is_ok()
}

/// Pick the backend for one machine and open it. The null fallback prints
/// exactly one warning naming the reason, as the plan requires.
pub fn select_backend(node_name: &str, node_description: &str) -> Arc<dyn HostAudio> {
    let probe = probe_environment();
    match choose_host(&probe) {
        HostChoice::PipeWire => {
            #[cfg(feature = "pipewire")]
            {
                match pipewire::PipeWireHost::new(node_name, node_description) {
                    Ok(host) => Arc::new(host),
                    // Fall through to the remaining order with the failure
                    // named: an unreachable server is the next backend's
                    // problem.
                    Err(e) => {
                        if probe.pulse_compiled && probe.pulse_server_set {
                            return select_pulse(node_name, node_description);
                        }
                        eprintln!(
                            "vmlab-hv: audio to the null backend: PipeWire startup failed ({e}); PulseAudio unavailable"
                        );
                        Arc::new(null::NullHost)
                    }
                }
            }
            #[cfg(not(feature = "pipewire"))]
            {
                let _ = (node_name, node_description);
                warn_null();
                Arc::new(null::NullHost)
            }
        }
        HostChoice::Pulse => select_pulse(node_name, node_description),
        HostChoice::Null => {
            warn_null();
            Arc::new(null::NullHost)
        }
    }
}

fn select_pulse(node_name: &str, node_description: &str) -> Arc<dyn HostAudio> {
    #[cfg(feature = "pulse")]
    {
        Arc::new(pulse::PulseHost::new(node_name, node_description))
    }
    #[cfg(not(feature = "pulse"))]
    {
        let _ = (node_name, node_description);
        warn_null();
        Arc::new(null::NullHost)
    }
}

fn warn_null() {
    if let Some(reason) = HostChoice::Null.null_reason() {
        eprintln!("vmlab-hv: audio to the null backend: {reason}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frame_bytes_scales_with_format_and_channels() {
        let stereo16 = AudioFormat {
            sample_format: SampleFormat::S16Le,
            channels: 2,
            rate: 48000,
        };
        assert_eq!(stereo16.frame_bytes(), 4);
        let mono32 = AudioFormat {
            sample_format: SampleFormat::F32Le,
            channels: 1,
            rate: 44100,
        };
        assert_eq!(mono32.frame_bytes(), 4);
    }

    #[test]
    fn selection_prefers_a_reachable_pipewire_socket() {
        let probe = HostProbe {
            pipewire_compiled: true,
            pipewire_socket_reachable: true,
            pulse_compiled: true,
            pulse_server_set: true,
        };
        assert_eq!(choose_host(&probe), HostChoice::PipeWire);
    }

    #[test]
    fn selection_falls_back_to_pulse_without_pipewire() {
        let probe = HostProbe {
            pipewire_compiled: true,
            pipewire_socket_reachable: false,
            pulse_compiled: true,
            pulse_server_set: true,
        };
        assert_eq!(choose_host(&probe), HostChoice::Pulse);
        // A compiled-out pipewire backend never wins on socket presence
        // alone.
        let probe = HostProbe {
            pipewire_compiled: false,
            pipewire_socket_reachable: true,
            pulse_compiled: true,
            pulse_server_set: true,
        };
        assert_eq!(choose_host(&probe), HostChoice::Pulse);
    }

    #[test]
    fn selection_lands_on_null_with_a_reason() {
        let probe = HostProbe {
            pipewire_compiled: true,
            pipewire_socket_reachable: false,
            pulse_compiled: true,
            pulse_server_set: false,
        };
        assert_eq!(choose_host(&probe), HostChoice::Null);
        assert!(HostChoice::Null.null_reason().is_some());
        assert_eq!(HostChoice::PipeWire.null_reason(), None);
    }

    #[test]
    fn select_backend_returns_a_usable_host_without_a_sound_server() {
        // Under `cargo test` XDG_RUNTIME_DIR may well point at a live
        // PipeWire, so assert only the invariant: selection always yields
        // a working backend object.
        let host = select_backend("vmlab.test.m", "vmlab test m");
        let format = AudioFormat {
            sample_format: SampleFormat::S16Le,
            channels: 2,
            rate: 48000,
        };
        let mut sink = host.open_playback(format).unwrap();
        sink.write_period(&[0u8; 256]);
        let mut source = host.open_capture(format).unwrap();
        let mut pcm = [0u8; 256];
        source.read_period(&mut pcm);
    }
}
