//! The recording backend, for tests (seam A1).
//!
//! [`RecordingHost`] records every playback period it receives, in order,
//! with a monotonic timestamp, and serves capture from a scripted list of
//! periods followed by silence. Tests use it to assert exactly which PCM
//! the guest produced and which PCM the guest consumed, with no sound
//! server involved. It also remembers whether each direction was opened
//! at all: the microphone-off requirement is checked here.

use parking_lot::Mutex;
use std::collections::VecDeque;
use std::sync::Arc;
use std::time::Instant;

use super::{AudioFormat, CaptureSource, HostAudio, PlaybackSink};

/// One recorded playback period.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecordedPeriod {
    pub pcm: Vec<u8>,
    /// When the period was accepted, monotonic like [`Instant`].
    pub at: Instant,
}

#[derive(Default)]
struct Shared {
    playback: Vec<RecordedPeriod>,
    playback_opened: bool,
    capture_opened: bool,
    capture_script: VecDeque<Vec<u8>>,
}

/// A host that records periods instead of playing them.
#[derive(Clone, Default)]
pub struct RecordingHost(Arc<Mutex<Shared>>);

impl RecordingHost {
    pub fn new() -> Self {
        Self::default()
    }

    /// Serve capture from `script`, one period per pull, then silence.
    pub fn with_capture_script(script: Vec<Vec<u8>>) -> Self {
        let host = Self::new();
        host.0.lock().capture_script = script.into();
        host
    }

    /// Every playback period received, in order.
    pub fn periods(&self) -> Vec<RecordedPeriod> {
        self.0.lock().playback.clone()
    }

    /// Raw PCM of every playback period, concatenated.
    pub fn playback_pcm(&self) -> Vec<u8> {
        self.0
            .lock()
            .playback
            .iter()
            .flat_map(|p| p.pcm.iter().copied())
            .collect()
    }

    /// Whether a playback stream was ever opened.
    pub fn playback_opened(&self) -> bool {
        self.0.lock().playback_opened
    }

    /// Whether a capture stream was ever opened. With the microphone off
    /// this must stay false.
    pub fn capture_opened(&self) -> bool {
        self.0.lock().capture_opened
    }
}

impl HostAudio for RecordingHost {
    fn name(&self) -> &'static str {
        "recording"
    }

    fn open_playback(
        &self,
        _format: AudioFormat,
    ) -> Result<Box<dyn PlaybackSink>, crate::DeviceError> {
        let mut shared = self.0.lock();
        shared.playback_opened = true;
        Ok(Box::new(RecordingSink(self.0.clone())))
    }

    fn open_capture(
        &self,
        _format: AudioFormat,
    ) -> Result<Box<dyn CaptureSource>, crate::DeviceError> {
        let mut shared = self.0.lock();
        shared.capture_opened = true;
        Ok(Box::new(ScriptedSource(self.0.clone())))
    }
}

struct RecordingSink(Arc<Mutex<Shared>>);

impl PlaybackSink for RecordingSink {
    fn write_period(&mut self, pcm: &[u8]) {
        self.0.lock().playback.push(RecordedPeriod {
            pcm: pcm.to_vec(),
            at: Instant::now(),
        });
    }

    fn latency_bytes(&self) -> usize {
        0
    }
}

struct ScriptedSource(Arc<Mutex<Shared>>);

impl CaptureSource for ScriptedSource {
    fn read_period(&mut self, out: &mut [u8]) {
        let period = self.0.lock().capture_script.pop_front();
        match period {
            Some(pcm) => {
                let n = out.len().min(pcm.len());
                out[..n].copy_from_slice(&pcm[..n]);
                out[n..].fill(0);
            }
            None => out.fill(0),
        }
    }

    fn latency_bytes(&self) -> usize {
        0
    }
}

#[cfg(test)]
mod tests {
    use super::super::SampleFormat;
    use super::*;

    fn format() -> AudioFormat {
        AudioFormat {
            sample_format: SampleFormat::S16Le,
            channels: 2,
            rate: 48000,
        }
    }

    #[test]
    fn playback_periods_are_recorded_in_order_with_monotonic_times() {
        let host = RecordingHost::new();
        let mut sink = host.open_playback(format()).unwrap();
        sink.write_period(&[1, 1]);
        sink.write_period(&[2, 2]);
        sink.write_period(&[3, 3]);
        let periods = host.periods();
        assert_eq!(periods.len(), 3);
        assert_eq!(periods[0].pcm, vec![1, 1]);
        assert_eq!(periods[1].pcm, vec![2, 2]);
        assert_eq!(periods[2].pcm, vec![3, 3]);
        assert!(periods[0].at <= periods[1].at);
        assert!(periods[1].at <= periods[2].at);
        assert!(host.playback_opened());
        assert_eq!(host.playback_pcm(), vec![1, 1, 2, 2, 3, 3]);
    }

    #[test]
    fn capture_serves_the_script_then_silence() {
        let host = RecordingHost::with_capture_script(vec![vec![9, 9], vec![8, 8]]);
        let mut source = host.open_capture(format()).unwrap();
        let mut pcm = [0u8; 2];
        source.read_period(&mut pcm);
        assert_eq!(pcm, [9, 9]);
        source.read_period(&mut pcm);
        assert_eq!(pcm, [8, 8]);
        source.read_period(&mut pcm);
        assert_eq!(pcm, [0, 0]);
        assert!(host.capture_opened());
    }

    #[test]
    fn opening_neither_stream_keeps_both_flags_clear() {
        let host = RecordingHost::new();
        assert!(!host.playback_opened());
        assert!(!host.capture_opened());
    }
}
