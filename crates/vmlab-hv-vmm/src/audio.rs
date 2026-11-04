//! Host-audio backend selection for one machine's audio device.
//!
//! The selection order is the audio plan's (hv-audio D4): PipeWire when its
//! socket under `XDG_RUNTIME_DIR` is reachable, else PulseAudio when
//! `PULSE_SERVER` is set, else null with one warning line. The backends
//! themselves live in `vmlab-hv-devices`; this module only picks one per
//! machine from the description's audio section.
//!
//! Test builds (`testing` feature) may replace the whole order with the
//! file-backed recording host below, so seam A3 can observe exactly which
//! PCM the guest produced and script what it captures, without a sound
//! server. Production builds compile none of it.

use std::sync::Arc;

use vmlab_hv_devices::audio::select_backend;
use vmlab_hv_proto::machine::Audio;

/// Pick and open the host backend for one machine's audio device.
pub fn host(section: &Audio) -> Arc<dyn HostAudio> {
    #[cfg(feature = "testing")]
    {
        let playback = std::env::var_os("VMLAB_HV_TEST_AUDIO_PLAYBACK");
        let capture = std::env::var_os("VMLAB_HV_TEST_AUDIO_CAPTURE");
        if playback.is_some() || capture.is_some() {
            return Arc::new(FileRecordingHost {
                playback: playback.map(std::path::PathBuf::from),
                capture: capture.map(std::path::PathBuf::from),
                downstream: std::env::var_os("VMLAB_HV_TEST_AUDIO_TEE")
                    .map(|_| select_backend(&section.node_name, &section.node_description)),
            });
        }
    }
    select_backend(&section.node_name, &section.node_description)
}

#[cfg(feature = "testing")]
mod file_recording {
    //! The test observation channel (seam A1's recording idea, file-backed
    //! so a separate test process can read it):
    //!
    //! - `VMLAB_HV_TEST_AUDIO_PLAYBACK=<file>`: every playback period is
    //!   appended raw, in order, and every opened stream appends one line to
    //!   `<file>.events` (`playback <rate> <channels> <encoding>` or
    //!   `capture ...`). With the microphone off no `capture` line may ever
    //!   appear — that is the A3 assertion.
    //! - `VMLAB_HV_TEST_AUDIO_CAPTURE=<file>`: the file's bytes are served
    //!   as capture PCM, in order, then silence.

    use std::collections::VecDeque;
    use std::fs::OpenOptions;
    use std::io::Write;
    use std::path::PathBuf;
    use std::sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    };
    use std::time::{Duration, Instant};

    use parking_lot::Mutex;

    use vmlab_hv_devices::DeviceError;
    use vmlab_hv_devices::audio::{
        AudioFormat, CaptureSource, HostAudio, PlaybackSink, SampleFormat,
    };

    pub struct FileRecordingHost {
        pub playback: Option<PathBuf>,
        pub capture: Option<PathBuf>,
        pub downstream: Option<Arc<dyn HostAudio>>,
    }

    impl HostAudio for FileRecordingHost {
        fn name(&self) -> &'static str {
            "recording"
        }
        fn open_playback(&self, format: AudioFormat) -> Result<Box<dyn PlaybackSink>, DeviceError> {
            if let Some(path) = &self.playback {
                log_event(path, "playback", format);
            }
            let paced = self.downstream.is_none()
                && std::env::var_os("VMLAB_HV_TEST_AUDIO_PACED").is_some();
            let mut sink = FileSink::new(self.playback.clone(), format, paced)?;
            sink.downstream = self
                .downstream
                .as_ref()
                .map(|host| host.open_playback(format))
                .transpose()?;
            Ok(Box::new(sink))
        }
        fn open_capture(&self, format: AudioFormat) -> Result<Box<dyn CaptureSource>, DeviceError> {
            let mut script = Vec::new();
            if let Some(path) = &self.capture {
                script = std::fs::read(path)
                    .map_err(|e| DeviceError::Other(format!("capture script {path:?}: {e}")))?;
            }
            if let Some(path) = &self.playback {
                log_event(path, "capture", format);
            }
            Ok(Box::new(FileSource {
                script: Mutex::new(script),
            }))
        }
    }

    struct FileSink {
        file: Option<std::fs::File>,
        timing: Option<std::fs::File>,
        start: Instant,
        offset: usize,
        queue: Option<Arc<Mutex<VecDeque<u8>>>>,
        stop: Arc<AtomicBool>,
        worker: Option<std::thread::JoinHandle<()>>,
        downstream: Option<Box<dyn PlaybackSink>>,
    }

    impl FileSink {
        fn new(
            path: Option<PathBuf>,
            format: AudioFormat,
            paced: bool,
        ) -> Result<Self, DeviceError> {
            let open =
                |path: &std::path::Path| OpenOptions::new().create(true).append(true).open(path);
            let file = path.as_deref().map(open).transpose()?;
            let timing = if std::env::var_os("VMLAB_HV_TEST_AUDIO_TIMING").is_some() {
                path.as_deref()
                    .map(|path| open(&path.with_extension("timing")))
                    .transpose()?
            } else {
                None
            };
            let rendered = if paced {
                Some(open(
                    &path
                        .as_ref()
                        .ok_or_else(|| {
                            DeviceError::Other("paced recording requires a playback file".into())
                        })?
                        .with_extension("rendered"),
                )?)
            } else {
                None
            };
            let stop = Arc::new(AtomicBool::new(false));
            let queue = paced.then(|| Arc::new(Mutex::new(VecDeque::new())));
            let worker = queue.as_ref().map(|queue| {
                let queue = queue.clone();
                let stop = stop.clone();
                let mut rendered = rendered.expect("paced file was opened");
                std::thread::spawn(move || {
                    let period = Duration::from_secs_f64(256.0 / f64::from(format.rate));
                    // Prebuffer one callback period, then start the host clock.
                    while queue.lock().len() < 256 * format.frame_bytes() {
                        if stop.load(Ordering::Acquire) {
                            return;
                        }
                        std::thread::sleep(Duration::from_millis(1));
                    }
                    let mut deadline = Instant::now() + period;
                    while !stop.load(Ordering::Acquire) {
                        std::thread::sleep(deadline.saturating_duration_since(Instant::now()));
                        let mut pcm = vec![0; 256 * format.frame_bytes()];
                        let mut queue = queue.lock();
                        for byte in &mut pcm {
                            *byte = queue.pop_front().unwrap_or(0);
                        }
                        drop(queue);
                        rendered.write_all(&pcm).expect("write paced PCM");
                        deadline += period;
                    }
                })
            });
            Ok(Self {
                offset: path
                    .as_ref()
                    .and_then(|p| std::fs::metadata(p).ok())
                    .map_or(0, |m| m.len() as usize),
                file,
                timing,
                start: *START,
                queue,
                stop,
                worker,
                downstream: None,
            })
        }
    }

    static START: std::sync::LazyLock<Instant> = std::sync::LazyLock::new(Instant::now);

    impl Drop for FileSink {
        fn drop(&mut self) {
            self.stop.store(true, Ordering::Release);
            if let Some(worker) = self.worker.take() {
                let _ = worker.join();
            }
        }
    }

    impl PlaybackSink for FileSink {
        fn write_period(&mut self, pcm: &[u8]) {
            if let Some(file) = &mut self.file {
                file.write_all(pcm).expect("write test PCM");
                if let Some(timing) = &mut self.timing {
                    let line = serde_json::json!({
                        "offset": self.offset, "bytes": pcm.len(),
                        "ns": self.start.elapsed().as_nanos() as u64
                    })
                    .to_string()
                        + "\n";
                    timing.write_all(line.as_bytes()).expect("write PCM timing");
                }
            }
            self.offset += pcm.len();
            if let Some(queue) = &self.queue {
                queue.lock().extend(pcm);
            }
            if let Some(sink) = &mut self.downstream {
                sink.write_period(pcm);
            }
        }
        fn latency_bytes(&self) -> usize {
            if let Some(sink) = &self.downstream {
                return sink.latency_bytes();
            }
            self.queue.as_ref().map_or(0, |queue| queue.lock().len())
        }
        fn queued_bytes(&self) -> usize {
            self.downstream
                .as_ref()
                .map_or_else(|| self.latency_bytes(), |sink| sink.queued_bytes())
        }
    }

    struct FileSource {
        script: Mutex<Vec<u8>>,
    }

    impl CaptureSource for FileSource {
        fn read_period(&mut self, out: &mut [u8]) {
            let mut script = self.script.lock();
            let n = out.len().min(script.len());
            out[..n].copy_from_slice(&script[..n]);
            out[n..].fill(0);
            script.drain(..n);
        }
        fn latency_bytes(&self) -> usize {
            0
        }
    }

    fn append(path: &std::path::Path, bytes: &[u8]) {
        let result = OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .and_then(|mut file| file.write_all(bytes));
        if let Err(e) = result {
            log::warn!("vmlab-hv: test audio dump {}: {e}", path.display());
        }
    }

    fn log_event(playback_path: &std::path::Path, what: &str, format: AudioFormat) {
        let encoding = match format.sample_format {
            SampleFormat::S16Le => "s16le",
            SampleFormat::F32Le => "f32le",
        };
        let mut events = playback_path.as_os_str().to_owned();
        events.push(".events");
        let line = format!("{what} {} {} {encoding}\n", format.rate, format.channels);
        append(std::path::Path::new(&events), line.as_bytes());
    }
}

#[cfg(feature = "testing")]
use file_recording::FileRecordingHost;
use vmlab_hv_devices::audio::HostAudio;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn selects_a_backend_without_touching_the_environment() {
        // The null fallback always constructs; nothing here may panic or
        // block, whatever the host's sound setup is.
        let host = host(&Audio {
            pci: None,
            device: vmlab_hv_proto::machine::AudioDevice::Hda,
            microphone: false,
            node_name: "test".into(),
            node_description: "test".into(),
        });
        assert!(!host.name().is_empty());
    }
}
