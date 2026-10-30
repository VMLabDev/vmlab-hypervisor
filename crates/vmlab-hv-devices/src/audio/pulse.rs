//! PulseAudio fallback with one mainloop thread per stream direction.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc::{self, Sender};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use libpulse_binding as pa;

use super::{AudioFormat, CaptureSource, HostAudio, PlaybackSink, SampleFormat, ring::SpscRing};
use crate::DeviceError;

const START_TIMEOUT: Duration = Duration::from_secs(3);
const POLL_INTERVAL: Duration = Duration::from_millis(1);

pub struct PulseHost {
    node_name: String,
    node_description: String,
}

impl PulseHost {
    pub fn new(node_name: &str, node_description: &str) -> Self {
        Self {
            node_name: node_name.into(),
            node_description: node_description.into(),
        }
    }

    fn open(&self, format: AudioFormat, capture: bool) -> Result<Endpoint, DeviceError> {
        if !(1..=2).contains(&format.channels)
            || format.rate == 0
            || self.node_name.contains('\0')
            || self.node_description.contains('\0')
        {
            return Err(error("invalid PCM format or node metadata"));
        }
        let shared = Arc::new(Shared {
            ring: SpscRing::with_capacity_bytes(4 * 256 * format.frame_bytes()),
            stop: AtomicBool::new(false),
            alive: AtomicBool::new(true),
            host_latency: AtomicUsize::new(0),
        });
        let (tx, rx) = mpsc::channel();
        let name = self.node_name.clone();
        let description = self.node_description.clone();
        let worker = shared.clone();
        let join = thread::Builder::new()
            .name(
                if capture {
                    "pulse-capture"
                } else {
                    "pulse-playback"
                }
                .into(),
            )
            .spawn(move || {
                if let Err(e) = run(&name, &description, format, capture, &worker, &tx) {
                    let _ = tx.send(Err(e.to_string()));
                }
                worker.alive.store(false, Ordering::Release);
            })?;
        let endpoint = Endpoint {
            shared,
            join: Some(join),
        };
        match rx.recv_timeout(START_TIMEOUT + POLL_INTERVAL) {
            Ok(Ok(())) => Ok(endpoint),
            Ok(Err(e)) => Err(error(e)),
            Err(e) => Err(error(format!("stream startup: {e}"))),
        }
    }
}

impl HostAudio for PulseHost {
    fn name(&self) -> &'static str {
        "pulse"
    }

    fn open_playback(&self, format: AudioFormat) -> Result<Box<dyn PlaybackSink>, DeviceError> {
        Ok(Box::new(self.open(format, false)?))
    }

    fn open_capture(&self, format: AudioFormat) -> Result<Box<dyn CaptureSource>, DeviceError> {
        Ok(Box::new(self.open(format, true)?))
    }
}

fn error(e: impl std::fmt::Display) -> DeviceError {
    DeviceError::Other(format!("PulseAudio: {e}"))
}

struct Shared {
    ring: SpscRing,
    stop: AtomicBool,
    alive: AtomicBool,
    host_latency: AtomicUsize,
}

struct Endpoint {
    shared: Arc<Shared>,
    join: Option<JoinHandle<()>>,
}

impl Drop for Endpoint {
    fn drop(&mut self) {
        self.shared.stop.store(true, Ordering::Release);
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}

impl PlaybackSink for Endpoint {
    fn queued_bytes(&self) -> usize {
        if self.shared.alive.load(Ordering::Acquire) {
            self.shared.ring.used_bytes()
        } else {
            0
        }
    }

    fn write_period(&mut self, pcm: &[u8]) {
        if self.shared.alive.load(Ordering::Acquire) {
            self.shared.ring.push_drop_oldest(pcm);
        }
    }

    fn latency_bytes(&self) -> usize {
        if !self.shared.alive.load(Ordering::Acquire) {
            return 0;
        }
        self.shared
            .ring
            .used_bytes()
            .saturating_add(self.shared.host_latency.load(Ordering::Relaxed))
    }
}

impl CaptureSource for Endpoint {
    fn read_period(&mut self, out: &mut [u8]) {
        let n = self.shared.ring.pop_max(out);
        out[n..].fill(0);
    }

    fn latency_bytes(&self) -> usize {
        self.shared.ring.used_bytes()
    }
}

fn iterate(mainloop: &mut pa::mainloop::standard::Mainloop) -> Result<(), DeviceError> {
    match mainloop.iterate(false) {
        pa::mainloop::standard::IterateResult::Success(_) => Ok(()),
        other => Err(error(format!("mainloop ended: {other:?}"))),
    }
}

fn run(
    name: &str,
    description: &str,
    format: AudioFormat,
    capture: bool,
    shared: &Shared,
    ready: &Sender<Result<(), String>>,
) -> Result<(), DeviceError> {
    let mut mainloop =
        pa::mainloop::standard::Mainloop::new().ok_or_else(|| error("create mainloop"))?;
    let mut props = pa::proplist::Proplist::new().ok_or_else(|| error("create properties"))?;
    for (key, value) in [
        ("application.name", name),
        ("application.id", name),
        ("media.name", description),
        ("node.name", name),
        ("node.description", description),
        ("node.latency", "256/48000"),
        ("media.role", "music"),
    ] {
        props
            .set_str(key, value)
            .map_err(|_| error("invalid node metadata"))?;
    }
    let mut context = pa::context::Context::new_with_proplist(&mainloop, name, &props)
        .ok_or_else(|| error("create context"))?;
    context
        .connect(None, pa::context::FlagSet::NOAUTOSPAWN, None)
        .map_err(error)?;
    let deadline = Instant::now() + START_TIMEOUT;
    while context.get_state() != pa::context::State::Ready {
        if shared.stop.load(Ordering::Acquire) || Instant::now() >= deadline {
            return Err(error("context startup cancelled or timed out"));
        }
        if matches!(
            context.get_state(),
            pa::context::State::Failed | pa::context::State::Terminated
        ) {
            return Err(error("context connection failed"));
        }
        iterate(&mut mainloop)?;
        thread::sleep(POLL_INTERVAL);
    }
    let spec = pa::sample::Spec {
        format: match format.sample_format {
            SampleFormat::S16Le => pa::sample::Format::S16le,
            SampleFormat::F32Le => pa::sample::Format::F32le,
        },
        channels: format.channels,
        rate: format.rate,
    };
    if !spec.is_valid() {
        return Err(error("unsupported PCM format"));
    }
    let mut stream =
        pa::stream::Stream::new_with_proplist(&mut context, description, &spec, None, &mut props)
            .ok_or_else(|| error("create stream"))?;
    // Express the fixed 256/48000 target in this stream's sample rate.
    let frames = (u64::from(format.rate) * 256).div_ceil(48000) as usize;
    let period = frames * format.frame_bytes();
    let attr = pa::def::BufferAttr {
        maxlength: (period * 4) as u32,
        tlength: period as u32,
        prebuf: 0,
        minreq: period as u32,
        fragsize: period as u32,
    };
    let flags = pa::stream::FlagSet::ADJUST_LATENCY
        | pa::stream::FlagSet::AUTO_TIMING_UPDATE
        | pa::stream::FlagSet::INTERPOLATE_TIMING;
    if capture {
        stream
            .connect_record(None, Some(&attr), flags)
            .map_err(error)?;
    } else {
        stream
            .connect_playback(None, Some(&attr), flags, None, None)
            .map_err(error)?;
    }
    let mut started = false;
    let mut scratch = vec![0u8; period];
    while !shared.stop.load(Ordering::Acquire) {
        iterate(&mut mainloop)?;
        if matches!(
            context.get_state(),
            pa::context::State::Failed | pa::context::State::Terminated
        ) {
            return Err(error("server disconnected"));
        }
        match stream.get_state() {
            pa::stream::State::Failed | pa::stream::State::Terminated => {
                return Err(error("stream disconnected"));
            }
            pa::stream::State::Ready => {
                if !started {
                    let _ = ready.send(Ok(()));
                    started = true;
                }
                if capture {
                    // Bound work per iteration so stop cannot starve under a busy source.
                    for _ in 0..16 {
                        match stream.peek().map_err(error)? {
                            pa::stream::PeekResult::Empty => break,
                            pa::stream::PeekResult::Data(bytes) => {
                                // Keep pushes below ring capacity, including large server fragments.
                                for part in bytes.chunks(period) {
                                    shared.ring.push_drop_oldest(part);
                                }
                            }
                            pa::stream::PeekResult::Hole(mut size) => {
                                scratch.fill(0);
                                while size > 0 {
                                    let n = size.min(scratch.len());
                                    shared.ring.push_drop_oldest(&scratch[..n]);
                                    size -= n;
                                }
                            }
                        }
                        stream.discard().map_err(error)?;
                    }
                } else {
                    let writable = stream.writable_size().unwrap_or(0).min(period * 4);
                    let mut remaining = writable / format.frame_bytes() * format.frame_bytes();
                    while remaining > 0 {
                        let n = remaining.min(scratch.len());
                        let copied = shared.ring.pop_max(&mut scratch[..n]);
                        scratch[copied..n].fill(0);
                        stream
                            .write_copy(&scratch[..n], 0, pa::stream::SeekMode::Relative)
                            .map_err(error)?;
                        remaining -= n;
                    }
                }
                let micros = match stream.get_latency() {
                    Ok(pa::stream::Latency::Positive(value)) => value.0,
                    _ => 0,
                };
                let bytes =
                    u128::from(micros) * u128::from(format.rate) * format.frame_bytes() as u128
                        / 1_000_000;
                shared
                    .host_latency
                    .store(bytes.min(usize::MAX as u128) as usize, Ordering::Relaxed);
            }
            _ => {}
        }
        if !started && Instant::now() >= deadline {
            return Err(error("stream startup timed out"));
        }
        thread::sleep(POLL_INTERVAL);
    }
    stream.disconnect().map_err(error)?;
    context.disconnect();
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn graph_latency_does_not_count_as_queued_pcm() {
        let mut sink = Endpoint {
            shared: Arc::new(Shared {
                ring: SpscRing::with_capacity_bytes(4096),
                stop: AtomicBool::new(false),
                alive: AtomicBool::new(true),
                host_latency: AtomicUsize::new(8192),
            }),
            join: None,
        };
        assert_eq!(sink.queued_bytes(), 0);
        assert_eq!(PlaybackSink::latency_bytes(&sink), 8192);
        sink.write_period(&[1; 1024]);
        assert_eq!(sink.queued_bytes(), 1024);
        assert_eq!(PlaybackSink::latency_bytes(&sink), 9216);
        sink.shared.alive.store(false, Ordering::Release);
        assert_eq!(sink.queued_bytes(), 0);
        assert_eq!(PlaybackSink::latency_bytes(&sink), 0);
    }

    #[test]
    fn real_pulse_streams() {
        let server_set = std::env::var_os("PULSE_SERVER").is_some_and(|v| !v.is_empty());
        let socket_reachable = std::env::var_os("XDG_RUNTIME_DIR").is_some_and(|dir| {
            std::os::unix::net::UnixStream::connect(std::path::Path::new(&dir).join("pulse/native"))
                .is_ok()
        });
        if !server_set && !socket_reachable {
            eprintln!(
                "SKIP real_pulse_streams: PULSE_SERVER unset and local PulseAudio socket unavailable"
            );
            return;
        }
        let host = PulseHost::new("vmlab.test.pulse", "vmlab PulseAudio smoke");
        for sample_format in [SampleFormat::S16Le, SampleFormat::F32Le] {
            for channels in [1, 2] {
                let format = AudioFormat {
                    sample_format,
                    channels,
                    rate: 48000,
                };
                let mut sink = host.open(format, false).unwrap();
                sink.write_period(&vec![0; 256 * format.frame_bytes()]);
                let deadline = Instant::now() + START_TIMEOUT;
                while sink.shared.ring.used_bytes() != 0 && Instant::now() < deadline {
                    thread::sleep(POLL_INTERVAL);
                }
                assert!(sink.shared.alive.load(Ordering::Acquire));
                assert_eq!(
                    sink.shared.ring.used_bytes(),
                    0,
                    "PulseAudio did not consume PCM"
                );
                let worker = sink.shared.clone();
                drop(sink);
                assert!(!worker.alive.load(Ordering::Acquire));
                let mut source = host.open(format, true).unwrap();
                let mut captured = vec![1; 256 * format.frame_bytes()];
                source.read_period(&mut captured);
                let worker = source.shared.clone();
                drop(source);
                assert!(!worker.alive.load(Ordering::Acquire));
            }
        }
    }

    #[test]
    fn rejects_invalid_formats_and_metadata_without_starting_a_thread() {
        let host = PulseHost::new("vmlab.test", "test");
        for (channels, rate) in [(0, 48000), (3, 48000), (2, 0)] {
            assert!(
                host.open(
                    AudioFormat {
                        sample_format: SampleFormat::S16Le,
                        channels,
                        rate
                    },
                    false
                )
                .is_err()
            );
        }
        let host = PulseHost::new("invalid\0name", "test");
        assert!(
            host.open(
                AudioFormat {
                    sample_format: SampleFormat::F32Le,
                    channels: 2,
                    rate: 48000
                },
                false
            )
            .is_err()
        );
    }
}
