//! Native PipeWire streams, owned by one mainloop thread per direction.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc::{self, Sender};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use pipewire as pw;
use pw::{properties::properties, spa};

use super::{AudioFormat, CaptureSource, HostAudio, PlaybackSink, SampleFormat, ring::SpscRing};
use crate::DeviceError;

const START_TIMEOUT: Duration = Duration::from_secs(3);
const LOOP_TIMEOUT: Duration = Duration::from_millis(5);

pub struct PipeWireHost {
    node_name: String,
    node_description: String,
}

impl PipeWireHost {
    pub fn new(node_name: &str, node_description: &str) -> Result<Self, DeviceError> {
        if node_name.contains('\0') || node_description.contains('\0') {
            return Err(DeviceError::Other(
                "PipeWire node metadata contains NUL".into(),
            ));
        }
        pw::init();
        // Check the connection without creating a capture or playback node.
        let mainloop = pw::main_loop::MainLoopRc::new(None).map_err(error)?;
        let context = pw::context::ContextRc::new(&mainloop, None).map_err(error)?;
        let _core = context.connect_rc(None).map_err(error)?;
        Ok(Self {
            node_name: node_name.into(),
            node_description: node_description.into(),
        })
    }

    fn open(&self, format: AudioFormat, capture: bool) -> Result<Endpoint, DeviceError> {
        if !(1..=2).contains(&format.channels) || format.rate == 0 {
            return Err(error("invalid PCM format"));
        }
        let shared = Arc::new(Shared {
            ring: SpscRing::with_capacity_bytes(4 * 256 * format.frame_bytes()),
            stop: AtomicBool::new(false),
            alive: AtomicBool::new(true),
            host_latency: AtomicUsize::new(0),
            transferred: AtomicUsize::new(0),
        });
        let (tx, rx) = mpsc::channel();
        let name = self.node_name.clone();
        let description = self.node_description.clone();
        let worker = shared.clone();
        let join = thread::Builder::new()
            .name(if capture { "pw-capture" } else { "pw-playback" }.into())
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
        match rx.recv_timeout(START_TIMEOUT + LOOP_TIMEOUT) {
            Ok(Ok(())) => Ok(endpoint),
            Ok(Err(e)) => Err(error(e)),
            Err(e) => Err(error(format!("stream startup: {e}"))),
        }
    }
}

impl HostAudio for PipeWireHost {
    fn name(&self) -> &'static str {
        "pipewire"
    }

    fn open_playback(&self, format: AudioFormat) -> Result<Box<dyn PlaybackSink>, DeviceError> {
        Ok(Box::new(self.open(format, false)?))
    }

    fn open_capture(&self, format: AudioFormat) -> Result<Box<dyn CaptureSource>, DeviceError> {
        Ok(Box::new(self.open(format, true)?))
    }
}

fn error(e: impl std::fmt::Display) -> DeviceError {
    DeviceError::Other(format!("PipeWire: {e}"))
}

struct Shared {
    ring: SpscRing,
    stop: AtomicBool,
    alive: AtomicBool,
    host_latency: AtomicUsize,
    transferred: AtomicUsize,
}

struct Endpoint {
    shared: Arc<Shared>,
    join: Option<JoinHandle<()>>,
}

// pipewire 0.10's safe Buffer cannot set pw_buffer.size. Keep raw access here
// so queued timing uses the frame units recommended by pipewire/stream.h.
struct ProcessBuffer<'a> {
    raw: std::ptr::NonNull<pw::sys::pw_buffer>,
    stream: &'a pw::stream::Stream,
}

impl<'a> ProcessBuffer<'a> {
    fn dequeue(stream: &'a pw::stream::Stream) -> Option<Self> {
        // SAFETY: this callback runs on the stream's owning thread. The guard
        // returns this exclusively dequeued buffer before the callback ends.
        let raw = std::ptr::NonNull::new(unsafe { stream.dequeue_raw_buffer() })?;
        Some(Self { raw, stream })
    }

    fn requested(&self) -> u64 {
        // SAFETY: the guard owns a valid dequeued buffer.
        unsafe { self.raw.as_ref().requested }
    }

    fn set_frames(&mut self, frames: usize) {
        // SAFETY: only this guard accesses the dequeued buffer header.
        unsafe {
            self.raw.as_mut().size = frames as u64;
        }
    }

    fn data(&mut self) -> Option<&mut spa::buffer::Data> {
        // SAFETY: PipeWire owns the header for this dequeue's lifetime.
        let buffer = unsafe { self.raw.as_ref().buffer.as_mut() }?;
        if buffer.n_datas == 0 || buffer.datas.is_null() {
            return None;
        }
        // SAFETY: n_datas guarantees the first spa_data exists. Data is a
        // repr(transparent) wrapper. Its mutable borrow cannot outlive self.
        let data = unsafe { &mut *buffer.datas.cast::<spa::buffer::Data>() };
        if data.as_raw().chunk.is_null() {
            return None;
        }
        Some(data)
    }
}

impl Drop for ProcessBuffer<'_> {
    fn drop(&mut self) {
        // SAFETY: this is the same stream and exactly one matching dequeue.
        unsafe {
            self.stream.queue_raw_buffer(self.raw.as_ptr());
        }
    }
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

fn run(
    name: &str,
    description: &str,
    format: AudioFormat,
    capture: bool,
    shared: &Arc<Shared>,
    ready: &Sender<Result<(), String>>,
) -> Result<(), DeviceError> {
    let mainloop = pw::main_loop::MainLoopRc::new(None).map_err(error)?;
    let context = pw::context::ContextRc::new(&mainloop, None).map_err(error)?;
    let core = context.connect_rc(None).map_err(error)?;
    let failed = Arc::new(AtomicBool::new(false));
    let failed_callback = failed.clone();
    let _core_listener = core
        .add_listener_local()
        .error(move |id, _, _, _| {
            if id == 0 {
                failed_callback.store(true, Ordering::Release);
            }
        })
        .register();
    let stream = pw::stream::StreamBox::new(
        &core,
        name,
        properties! {
            "media.type" => "Audio",
            "media.category" => if capture { "Capture" } else { "Playback" },
            "media.role" => "Music",
            "node.name" => name,
            "node.description" => description,
            "node.latency" => "256/48000",
        },
    )
    .map_err(error)?;
    let stride = format.frame_bytes();
    let worker = shared.clone();
    let _listener = stream
        .add_local_listener_with_user_data(worker)
        .process(move |stream, shared| {
            let Some(mut buffer) = ProcessBuffer::dequeue(stream) else {
                return;
            };
            let requested = buffer.requested() as usize;
            let Some(data) = buffer.data() else {
                return;
            };
            if capture {
                let offset = data.chunk().offset() as usize;
                let size = data.chunk().size() as usize;
                if let Some(bytes) = data.data() {
                    let offset = offset.min(bytes.len());
                    let size = size.min(bytes.len() - offset) / stride * stride;
                    for period in bytes[offset..offset + size].chunks(256 * stride) {
                        shared.ring.push_drop_oldest(period);
                    }
                    shared.transferred.fetch_add(size, Ordering::Relaxed);
                }
            } else {
                let size = if let Some(bytes) = data.data() {
                    let frames = bytes.len() / stride;
                    let frames = if requested == 0 {
                        frames.min(256)
                    } else {
                        frames.min(requested)
                    };
                    let size = frames * stride;
                    let n = shared.ring.pop_max(&mut bytes[..size]);
                    bytes[n..size].fill(0);
                    shared.transferred.fetch_add(n, Ordering::Relaxed);
                    size
                } else {
                    0
                };
                *data.chunk_mut().offset_mut() = 0;
                *data.chunk_mut().stride_mut() = stride as i32;
                *data.chunk_mut().size_mut() = size as u32;
                buffer.set_frames(size / stride);
            }
            if let Ok(time) = stream.time() {
                let rate = time.rate();
                let delay = (time.delay().max(0) as u128)
                    .saturating_mul(rate.num as u128)
                    .saturating_mul(format.rate as u128)
                    / u128::from(rate.denom.max(1));
                let bytes = delay
                    .saturating_add(time.buffered() as u128)
                    .saturating_add(time.queued() as u128)
                    .saturating_mul(stride as u128);
                shared
                    .host_latency
                    .store(bytes.min(usize::MAX as u128) as usize, Ordering::Relaxed);
            }
        })
        .register()
        .map_err(error)?;
    let mut info = spa::param::audio::AudioInfoRaw::new();
    info.set_format(match format.sample_format {
        SampleFormat::S16Le => spa::param::audio::AudioFormat::S16LE,
        SampleFormat::F32Le => spa::param::audio::AudioFormat::F32LE,
    });
    info.set_rate(format.rate);
    info.set_channels(format.channels.into());
    let mut positions = [0; spa::param::audio::MAX_CHANNELS];
    if format.channels == 1 {
        positions[0] = spa::sys::SPA_AUDIO_CHANNEL_MONO;
    } else {
        positions[0] = spa::sys::SPA_AUDIO_CHANNEL_FL;
        positions[1] = spa::sys::SPA_AUDIO_CHANNEL_FR;
    }
    info.set_position(positions);
    let pod = spa::pod::Value::Object(spa::pod::Object {
        type_: spa::utils::SpaTypes::ObjectParamFormat.as_raw(),
        id: spa::param::ParamType::EnumFormat.as_raw(),
        properties: info.into(),
    });
    let bytes =
        spa::pod::serialize::PodSerializer::serialize(std::io::Cursor::new(Vec::new()), &pod)
            .map_err(|e| error(format!("format serialization: {e:?}")))?
            .0
            .into_inner();
    let mut params =
        [spa::pod::Pod::from_bytes(&bytes).ok_or_else(|| error("invalid format pod"))?];
    // Without RT_PROCESS, callbacks stay on this owning mainloop thread.
    stream
        .connect(
            if capture {
                spa::utils::Direction::Input
            } else {
                spa::utils::Direction::Output
            },
            None,
            pw::stream::StreamFlags::AUTOCONNECT | pw::stream::StreamFlags::MAP_BUFFERS,
            &mut params,
        )
        .map_err(error)?;
    let deadline = Instant::now() + START_TIMEOUT;
    let mut started = false;
    while !shared.stop.load(Ordering::Acquire) {
        if mainloop
            .loop_()
            .iterate(pw::loop_::Timeout::Finite(LOOP_TIMEOUT))
            < 0
        {
            return Err(error("mainloop failed"));
        }
        if failed.load(Ordering::Acquire) {
            return Err(error("server disconnected"));
        }
        match stream.state() {
            pw::stream::StreamState::Error(e) => return Err(error(e)),
            pw::stream::StreamState::Unconnected => return Err(error("stream disconnected")),
            pw::stream::StreamState::Paused | pw::stream::StreamState::Streaming if !started => {
                let _ = ready.send(Ok(()));
                started = true;
            }
            _ => {}
        }
        if !started && Instant::now() >= deadline {
            return Err(error("stream startup timed out"));
        }
    }
    stream.disconnect().map_err(error)?;
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
                transferred: AtomicUsize::new(0),
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
    fn real_pipewire_playback_smoke() {
        if !super::super::probe_environment().pipewire_socket_reachable {
            eprintln!(
                "SKIP real_pipewire_playback_smoke: PipeWire socket unavailable under XDG_RUNTIME_DIR"
            );
            return;
        }
        let host = PipeWireHost::new("vmlab.test.playback", "vmlab playback smoke").unwrap();
        let format = AudioFormat {
            sample_format: SampleFormat::S16Le,
            channels: 2,
            rate: 48000,
        };
        let mut sink = host.open(format, false).unwrap();
        let mut pcm = [0u8; 1024];
        for (i, frame) in pcm.as_chunks_mut::<4>().0.iter_mut().enumerate() {
            let sample =
                ((i as f32 * std::f32::consts::TAU * 440.0 / 48000.0).sin() * 1000.0) as i16;
            frame[..2].copy_from_slice(&sample.to_le_bytes());
            frame[2..].copy_from_slice(&sample.to_le_bytes());
        }
        let deadline = Instant::now() + START_TIMEOUT;
        while sink.shared.transferred.load(Ordering::Relaxed) == 0 && Instant::now() < deadline {
            sink.write_period(&pcm);
            thread::sleep(LOOP_TIMEOUT);
        }
        assert!(
            sink.shared.transferred.load(Ordering::Relaxed) > 0,
            "PipeWire connected but no playback process callback consumed PCM; check the default sink and session manager"
        );
        let worker = sink.shared.clone();
        drop(sink);
        assert!(!worker.alive.load(Ordering::Acquire));
    }

    #[test]
    fn real_pipewire_formats() {
        if !super::super::probe_environment().pipewire_socket_reachable {
            eprintln!(
                "SKIP real_pipewire_formats: PipeWire socket unavailable under XDG_RUNTIME_DIR"
            );
            return;
        }
        let host = PipeWireHost::new("vmlab.test.formats", "vmlab format smoke").unwrap();
        for sample_format in [SampleFormat::S16Le, SampleFormat::F32Le] {
            for channels in [1, 2] {
                let format = AudioFormat {
                    sample_format,
                    channels,
                    rate: 48000,
                };
                let mut sink = host.open(format, false).unwrap();
                let pcm = vec![0; 256 * format.frame_bytes()];
                let deadline = Instant::now() + START_TIMEOUT;
                while sink.shared.transferred.load(Ordering::Relaxed) == 0
                    && Instant::now() < deadline
                {
                    sink.write_period(&pcm);
                    thread::sleep(LOOP_TIMEOUT);
                }
                assert!(
                    sink.shared.transferred.load(Ordering::Relaxed) > 0,
                    "format {format:?} did not process PCM"
                );
            }
        }
    }

    struct Child(std::process::Child);

    impl Drop for Child {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }

    #[test]
    fn daemon_disconnect_joins_worker() {
        use std::process::{Command, Stdio};
        if let Some(marker) = std::env::var_os("VMLAB_AUDIO_DISCONNECT_CHILD") {
            let host = PipeWireHost::new("vmlab.test.disconnect", "disconnect test").unwrap();
            let format = AudioFormat {
                sample_format: SampleFormat::S16Le,
                channels: 2,
                rate: 48000,
            };
            let mut sink = host.open(format, false).unwrap();
            let worker = sink.shared.clone();
            std::fs::write(marker, b"ready").unwrap();
            let deadline = Instant::now() + START_TIMEOUT;
            while worker.alive.load(Ordering::Acquire) && Instant::now() < deadline {
                sink.write_period(&[0; 1024]);
                thread::sleep(LOOP_TIMEOUT);
            }
            assert!(
                !worker.alive.load(Ordering::Acquire),
                "worker survived daemon disconnect"
            );
            assert_eq!(PlaybackSink::latency_bytes(&sink), 0);
            let start = Instant::now();
            drop(sink);
            assert!(
                start.elapsed() < Duration::from_secs(1),
                "drop did not join promptly"
            );
            return;
        }
        if Command::new("pipewire").arg("--version").output().is_err() {
            eprintln!("SKIP daemon_disconnect_joins_worker: pipewire executable unavailable");
            return;
        }
        let temp = Command::new("mktemp").arg("-d").output().unwrap();
        assert!(temp.status.success());
        let directory = std::path::PathBuf::from(String::from_utf8(temp.stdout).unwrap().trim());
        let marker = directory.join("ready");
        let mut daemon = Child(
            Command::new("pipewire")
                .env("XDG_RUNTIME_DIR", &directory)
                .env("PIPEWIRE_RUNTIME_DIR", &directory)
                .env_remove("PIPEWIRE_REMOTE")
                .env_remove("PIPEWIRE_CORE")
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .unwrap(),
        );
        let socket = directory.join("pipewire-0");
        let deadline = Instant::now() + START_TIMEOUT;
        while std::os::unix::net::UnixStream::connect(&socket).is_err() && Instant::now() < deadline
        {
            assert!(
                daemon.0.try_wait().unwrap().is_none(),
                "isolated PipeWire daemon failed to start"
            );
            thread::sleep(LOOP_TIMEOUT);
        }
        assert!(socket.exists(), "isolated PipeWire socket did not appear");
        let mut client = Child(
            Command::new(std::env::current_exe().unwrap())
                .args([
                    "audio::pipewire::tests::daemon_disconnect_joins_worker",
                    "--exact",
                    "--nocapture",
                ])
                .env("XDG_RUNTIME_DIR", &directory)
                .env("PIPEWIRE_RUNTIME_DIR", &directory)
                .env("PIPEWIRE_REMOTE", "pipewire-0")
                .env("VMLAB_AUDIO_DISCONNECT_CHILD", &marker)
                .spawn()
                .unwrap(),
        );
        let deadline = Instant::now() + START_TIMEOUT + Duration::from_secs(1);
        while !marker.exists() && Instant::now() < deadline {
            assert!(
                client.0.try_wait().unwrap().is_none(),
                "disconnect client failed before opening stream"
            );
            thread::sleep(LOOP_TIMEOUT);
        }
        assert!(marker.exists(), "disconnect client did not open stream");
        daemon.0.kill().unwrap();
        daemon.0.wait().unwrap();
        let deadline = Instant::now() + START_TIMEOUT + Duration::from_secs(1);
        loop {
            if let Some(status) = client.0.try_wait().unwrap() {
                assert!(status.success(), "disconnect client failed: {status}");
                break;
            }
            assert!(
                Instant::now() < deadline,
                "disconnect client leaked its worker"
            );
            thread::sleep(LOOP_TIMEOUT);
        }
        drop(client);
        drop(daemon);
        std::fs::remove_dir_all(directory).unwrap();
    }
}
