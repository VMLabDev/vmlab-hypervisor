//! Timestamp every click in one PipeWire node's monitor stream (A4).
//!
//! ```text
//! monitor_tap <target-object> <seconds> <threshold>
//! ```
//!
//! Opens a capture stream against `<target-object>` (a monitor source,
//! e.g. `vmlab-a4.monitor`), prints `TAP-READY` once streaming, and then,
//! for every 16-bit left-channel sample whose magnitude crosses
//! `threshold` (at most one report per 300 ms), one line:
//!
//! ```text
//! CLICK <unix-ns> <frame-in-buffer>
//! ```
//!
//! The timestamp is the host clock at buffer arrival, corrected back by
//! the click's position inside the buffer, so the printed instant is the
//! moment the click physically left the tapped node (minus the monitor's
//! own one-quantum hop, which a real speaker path shares). Part of the A4
//! loopback latency harness; outside the merge bar.

use std::ptr::NonNull;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use pipewire as pw;
use pw::{properties::properties, spa};

const START_TIMEOUT: Duration = Duration::from_secs(5);
const LOOP_TIMEOUT: Duration = Duration::from_millis(5);
const RATE: u32 = 48000;

fn main() {
    let mut args = std::env::args().skip(1);
    let (Some(target), Ok(seconds), Ok(threshold)) = (
        args.next(),
        args.next().unwrap_or_default().parse::<u64>(),
        args.next().unwrap_or_default().parse::<i16>(),
    ) else {
        eprintln!("usage: monitor_tap <target-object> <seconds> <threshold>");
        std::process::exit(2);
    };
    if let Err(e) = run(&target, seconds, threshold) {
        eprintln!("monitor-tap: {e}");
        std::process::exit(1);
    }
}

/// One dequeued buffer, returned to the stream on drop, as the device
/// backend's ProcessBuffer does.
struct BufferGuard<'a> {
    raw: NonNull<pw::sys::pw_buffer>,
    stream: &'a pw::stream::Stream,
}

impl BufferGuard<'_> {
    fn data(&mut self) -> Option<&mut spa::buffer::Data> {
        // SAFETY: PipeWire owns the header for this dequeue's lifetime.
        let buffer = unsafe { self.raw.as_ref().buffer.as_mut() }?;
        if buffer.n_datas == 0 || buffer.datas.is_null() {
            return None;
        }
        // SAFETY: n_datas guarantees the first spa_data exists; Data is a
        // repr(transparent) wrapper and cannot outlive self.
        let data = unsafe { &mut *buffer.datas.cast::<spa::buffer::Data>() };
        if data.as_raw().chunk.is_null() {
            return None;
        }
        Some(data)
    }
}

impl Drop for BufferGuard<'_> {
    fn drop(&mut self) {
        // SAFETY: exactly the buffer dequeued for this guard.
        unsafe { self.stream.queue_raw_buffer(self.raw.as_ptr()) };
    }
}

fn run(target: &str, seconds: u64, threshold: i16) -> Result<(), String> {
    pw::init();
    let mainloop = pw::main_loop::MainLoopRc::new(None).map_err(e)?;
    let context = pw::context::ContextRc::new(&mainloop, None).map_err(e)?;
    let core = context.connect_rc(None).map_err(e)?;
    let stream = pw::stream::StreamBox::new(
        &core,
        "vmlab-a4-monitor-tap",
        properties! {
            "media.type" => "Audio",
            "media.category" => "Capture",
            "media.role" => "Music",
            "node.name" => "vmlab.a4.tap",
            "node.latency" => "256/48000",
            "target.object" => target,
        },
    )
    .map_err(e)?;
    // The last reported click, so one click's tail does not retrigger;
    // the harness leaves a quiet gap between rounds.
    let last = std::sync::atomic::AtomicU64::new(0);
    let _listener = stream
        .add_local_listener_with_user_data(last)
        .process(move |stream, last| {
            // SAFETY: this callback runs on the stream's owning thread and
            // the guard returns the buffer before the callback ends.
            let Some(raw) = NonNull::new(unsafe { stream.dequeue_raw_buffer() }) else {
                return;
            };
            let mut guard = BufferGuard { raw, stream };
            let Some(data) = guard.data() else {
                return;
            };
            let offset = data.chunk().offset() as usize;
            let size = data.chunk().size() as usize;
            let Some(bytes) = data.data() else {
                return;
            };
            if bytes.len() < offset + size {
                return;
            }
            let pcm = &bytes[offset..offset + size];
            let frames = pcm.len() / 4;
            let arrival = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos() as u64;
            let refractory = last
                .load(std::sync::atomic::Ordering::Relaxed)
                .saturating_add(300_000_000);
            if arrival <= refractory {
                return;
            }
            for frame in 0..frames {
                let sample = i16::from_le_bytes([pcm[frame * 4], pcm[frame * 4 + 1]]);
                if sample.abs() >= threshold {
                    // The buffer covers [arrival - frames, arrival]; the
                    // click sits frame frames into it.
                    let back = (frames - 1 - frame) as u128 * 1_000_000_000 / u128::from(RATE);
                    println!("CLICK {} {frame}", arrival.saturating_sub(back as u64));
                    last.store(arrival, std::sync::atomic::Ordering::Relaxed);
                    break;
                }
            }
        })
        .register()
        .map_err(|e| e.to_string())?;
    let mut info = spa::param::audio::AudioInfoRaw::new();
    info.set_format(spa::param::audio::AudioFormat::S16LE);
    info.set_rate(RATE);
    info.set_channels(2);
    let mut positions = [0; spa::param::audio::MAX_CHANNELS];
    positions[0] = spa::sys::SPA_AUDIO_CHANNEL_FL;
    positions[1] = spa::sys::SPA_AUDIO_CHANNEL_FR;
    info.set_position(positions);
    let pod = spa::pod::Value::Object(spa::pod::Object {
        type_: spa::utils::SpaTypes::ObjectParamFormat.as_raw(),
        id: spa::param::ParamType::EnumFormat.as_raw(),
        properties: info.into(),
    });
    let bytes =
        spa::pod::serialize::PodSerializer::serialize(std::io::Cursor::new(Vec::new()), &pod)
            .map_err(|e| format!("format serialization: {e:?}"))?
            .0
            .into_inner();
    let mut params = [spa::pod::Pod::from_bytes(&bytes).ok_or("invalid format pod")?];
    stream
        .connect(
            spa::utils::Direction::Input,
            None,
            pw::stream::StreamFlags::AUTOCONNECT | pw::stream::StreamFlags::MAP_BUFFERS,
            &mut params,
        )
        .map_err(e)?;
    let deadline = Instant::now() + START_TIMEOUT;
    while !matches!(stream.state(), pw::stream::StreamState::Streaming) {
        if Instant::now() >= deadline {
            return Err(format!(
                "tap never streamed (state {:?}, target {target:?})",
                stream.state()
            ));
        }
        if mainloop
            .loop_()
            .iterate(pw::loop_::Timeout::Finite(LOOP_TIMEOUT))
            < 0
        {
            return Err("mainloop failed".into());
        }
    }
    println!("TAP-READY");
    let deadline = Instant::now() + Duration::from_secs(seconds);
    while Instant::now() < deadline {
        if mainloop
            .loop_()
            .iterate(pw::loop_::Timeout::Finite(LOOP_TIMEOUT))
            < 0
        {
            return Err("mainloop failed".into());
        }
    }
    Ok(())
}

fn e(error: impl std::fmt::Display) -> String {
    format!("PipeWire: {error}")
}
