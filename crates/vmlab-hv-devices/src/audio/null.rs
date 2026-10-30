//! The null backend: discards playback, supplies silence for capture.

use super::{AudioFormat, CaptureSource, HostAudio, PlaybackSink};

/// A host that is always available and does nothing.
pub struct NullHost;

impl HostAudio for NullHost {
    fn name(&self) -> &'static str {
        "null"
    }

    fn open_playback(
        &self,
        _format: AudioFormat,
    ) -> Result<Box<dyn PlaybackSink>, crate::DeviceError> {
        Ok(Box::new(NullSink))
    }

    fn open_capture(
        &self,
        _format: AudioFormat,
    ) -> Result<Box<dyn CaptureSource>, crate::DeviceError> {
        Ok(Box::new(SilenceSource))
    }
}

struct NullSink;

impl PlaybackSink for NullSink {
    fn write_period(&mut self, _pcm: &[u8]) {}

    fn latency_bytes(&self) -> usize {
        0
    }
}

struct SilenceSource;

impl CaptureSource for SilenceSource {
    fn read_period(&mut self, out: &mut [u8]) {
        out.fill(0);
    }

    fn latency_bytes(&self) -> usize {
        0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn format() -> AudioFormat {
        AudioFormat {
            sample_format: super::super::SampleFormat::S16Le,
            channels: 2,
            rate: 48000,
        }
    }

    #[test]
    fn playback_is_discarded_with_no_latency() {
        let mut sink = NullHost.open_playback(format()).unwrap();
        sink.write_period(&[1, 2, 3, 4]);
        assert_eq!(sink.latency_bytes(), 0);
    }

    #[test]
    fn capture_is_silence() {
        let mut source = NullHost.open_capture(format()).unwrap();
        let mut pcm = [7u8; 8];
        source.read_period(&mut pcm);
        assert_eq!(pcm, [0u8; 8]);
        assert_eq!(source.latency_bytes(), 0);
    }
}
