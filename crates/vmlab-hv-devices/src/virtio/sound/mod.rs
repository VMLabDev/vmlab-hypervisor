//! virtio-snd, VIRTIO 1.3 §5.14, device type 25.
//!
//! One playback stream and — only when the machine description turns the
//! microphone on — one capture stream. No jacks, no channel maps, no
//! control elements. PCM crosses the [host-audio seam](crate::audio);
//! nothing here talks to a sound server.
//!
//! The queues are the device's intermediate buffer. A started output
//! stream drains the tx queue no faster than the host sink accepts PCM
//! (§5.14.6.6.3.1: the intermediate buffer holds at least `buffer_bytes`,
//! which the un-consumed messages plus the host ring do), so a completion
//! is an elapsed-period notification paced by real playback, and the
//! `latency_bytes` in every status is the host seam's answer at that
//! moment. A started input stream banks machine time — the machine clock
//! calls [`VirtioSnd::advance`] — and fills one rx message per
//! `period_bytes` of it, straight from the host source.
//!
//! The device is shared between the virtio-pci transport ([`SoundPci`])
//! and the machine's running-time clock ([`audio::AudioClock`]): the vCPU
//! path feeds it queue kicks, the timer path paces completions.

#[cfg(test)]
mod tests;

use std::io::{Read as _, Write as _};
use std::sync::Arc;
use std::time::Duration;

use parking_lot::Mutex;

use virtio_queue::{Queue, QueueState, QueueT};

use crate::audio::{AudioClock, AudioFormat, CaptureSource, HostAudio, PlaybackSink, SampleFormat};
use crate::{DeviceError, GuestMemory, Snapshot, VirtioDevice, VirtioInterrupt};

/// VIRTIO_ID_SOUND.
pub const VIRTIO_ID_SOUND: u32 = 25;

/// `VIRTIO_F_RING_EVENT_IDX`, offered so the guest can suppress kicks.
const F_RING_EVENT_IDX: u64 = 1 << 29;

/// Control, tx and rx queues carry work (§5.14.2); the event queue —
/// queues[1] — never carries device-written buffers, because no PCM
/// feature that would raise an event is offered.
const VQ_CONTROL: usize = 0;
const VQ_TX: usize = 2;
const VQ_RX: usize = 3;
const QUEUE_SIZE: u16 = 64;
const NUM_QUEUES: usize = 4;

/// Control request codes and status codes (§5.14.6).
const R_PCM_INFO: u32 = 0x0100;
const R_PCM_SET_PARAMS: u32 = 0x0101;
const R_PCM_PREPARE: u32 = 0x0102;
const R_PCM_RELEASE: u32 = 0x0103;
const R_PCM_START: u32 = 0x0104;
const R_PCM_STOP: u32 = 0x0105;

const S_OK: u32 = 0x8000;
const S_BAD_MSG: u32 = 0x8001;
const S_NOT_SUPP: u32 = 0x8002;
const S_IO_ERR: u32 = 0x8003;

/// The sample formats the seam carries, as `VIRTIO_SND_PCM_FMT_*` codes.
const PCM_FMT_S16: u8 = 5;
const PCM_FMT_FLOAT: u8 = 19;
/// The frame rates the streams offer, as `VIRTIO_SND_PCM_RATE_*` codes.
const PCM_RATE_44100: u8 = 6;
const PCM_RATE_48000: u8 = 7;
const PCM_RATE_96000: u8 = 10;

/// `sizeof(virtio_snd_config)`: jacks, streams, chmaps, controls.
const CONFIG_LEN: usize = 16;

/// `sizeof(virtio_snd_pcm_info)`.
const PCM_INFO_LEN: usize = 32;

/// The playback pacing watermark: host PCM the device refills up to. One
/// guest period, at least the 256 frames every backend ring is sized for
/// (four of them), leaving room for the push in flight.
fn queued_target(period_bytes: usize, frame_bytes: usize) -> usize {
    period_bytes.max(256 * frame_bytes)
}

/// The largest PCM push given to the host sink at once. A push larger
/// than a backend's whole ring would be dropped, so a message's PCM goes
/// in 512-frame pieces.
const PUSH_FRAMES: usize = 512;

/// Frames the tx drain may move per call, bounding catch-up work after a
/// stall the same way the HDA model caps its DMA.
const TX_FRAMES_PER_TICK: usize = 8192;

/// The PCM command lifecycle (§5.14.6.6.1), as the device tracks it.
#[derive(Clone, Copy, PartialEq, Eq, Default, Debug)]
enum PcmState {
    /// No parameters yet.
    #[default]
    Unused,
    /// SET_PARAMS done.
    Set,
    /// PREPARE done; host stream open, pre-buffering allowed.
    Prepared,
    /// START done; the device consumes I/O messages.
    Started,
    /// STOP done; I/O messages wait.
    Stopped,
    /// RELEASE done; host stream closed, parameters kept for the next
    /// PREPARE.
    Released,
}

/// Negotiated stream parameters, as the guest sent them.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct PcmParams {
    buffer_bytes: u32,
    period_bytes: u32,
    /// The feature bit map the guest selected; only 0 passes validation.
    features: u32,
    format: AudioFormat,
    /// The virtio rate code, kept for the snapshot.
    rate_code: u8,
}

impl PcmParams {
    /// Whole frames in one hardware period.
    fn period_frames(&self) -> usize {
        self.period_bytes as usize / self.format.frame_bytes()
    }
}

/// The host stream of a prepared PCM stream: one direction only.
enum HostStream {
    Playback(Box<dyn PlaybackSink>),
    Capture(Box<dyn CaptureSource>),
}

#[derive(Default)]
struct Stream {
    state: PcmState,
    params: Option<PcmParams>,
    host: Option<HostStream>,
    /// Capture only: whole frames of machine time owed by the host source.
    due_frames: u64,
    /// Playback only: whole frames of machine time the stream may deliver.
    /// Without it a fast guest would free-run its own ring: completions are
    /// elapsed-period notifications, so they may only advance at the rate
    /// the machine clock banks, never faster than real playback.
    tx_credit: u64,
    /// Sub-frame remainder of the frame accumulator, in ns × rate units.
    fraction: u64,
}

/// A virtio-snd device: one playback stream, plus one capture stream when
/// `microphone` is set. Construct with [`VirtioSnd::new_shared`].
pub struct VirtioSnd {
    id: String,
    host: Arc<dyn HostAudio>,
    config: [u8; CONFIG_LEN],
    features: u64,
    acked_features: u64,
    /// Present once `activate` ran; cleared by `reset`.
    mem: Option<GuestMemory>,
    interrupt: Option<Arc<dyn VirtioInterrupt>>,
    queues: Vec<Queue>,
    queue_sizes: Vec<u16>,
    /// Queue states restored before activation, applied by `activate`.
    pending_queues: Option<Vec<QueueState>>,
    /// Stream 0 plays (VIRTIO_SND_D_OUTPUT); stream 1, when it exists,
    /// records (VIRTIO_SND_D_INPUT). The capture stream only exists with
    /// the microphone on, which is the audio plan's D1 requirement.
    streams: Vec<Stream>,
    paused: bool,
}

impl VirtioSnd {
    /// `microphone` decides whether a second, input stream exists; the
    /// host capture stream is only ever opened for it (audio plan D1).
    pub fn new(id: impl Into<String>, host: Arc<dyn HostAudio>, microphone: bool) -> Self {
        let mut config = [0u8; CONFIG_LEN];
        config[4..8].copy_from_slice(&(1 + u32::from(microphone)).to_le_bytes());
        Self {
            id: id.into(),
            host,
            config,
            features: F_RING_EVENT_IDX,
            acked_features: F_RING_EVENT_IDX,
            mem: None,
            interrupt: None,
            queues: Vec::new(),
            queue_sizes: vec![QUEUE_SIZE; NUM_QUEUES],
            pending_queues: None,
            streams: (0..1 + usize::from(microphone))
                .map(|_| Stream::default())
                .collect(),
            paused: false,
        }
    }

    /// The device and the PCI-facing handle sharing it: the transport
    /// owns the [`SoundPci`], the machine clock holds the other side.
    pub fn new_shared(
        id: impl Into<String>,
        host: Arc<dyn HostAudio>,
        microphone: bool,
    ) -> (Arc<Mutex<Self>>, SoundPci) {
        let shared = Arc::new(Mutex::new(Self::new(id, host, microphone)));
        let queue_sizes = shared.lock().queue_sizes.clone();
        let pci = SoundPci {
            device: shared.clone(),
            queue_sizes,
        };
        (shared, pci)
    }

    fn stream_ok(&self, stream_id: u32) -> bool {
        (stream_id as usize) < self.streams.len()
    }

    /// Open the host stream of `stream_id` for its saved parameters. With
    /// `discard_capture`, one period of host capture is read and dropped:
    /// it was recorded before a restore, so the guest must not hear it.
    fn open_host(&mut self, stream_id: usize, discard_capture: bool) -> Result<(), DeviceError> {
        let format = self.streams[stream_id]
            .params
            .ok_or_else(|| {
                DeviceError::State(format!("{}: stream {stream_id} unprepared", self.id))
            })?
            .format;
        let host_stream = if stream_id == 0 {
            HostStream::Playback(self.host.open_playback(format)?)
        } else {
            let mut source = self.host.open_capture(format)?;
            if discard_capture {
                let frame_bytes = format.frame_bytes();
                source.read_period(&mut vec![0; 256 * frame_bytes]);
            }
            HostStream::Capture(source)
        };
        self.streams[stream_id].host = Some(host_stream);
        Ok(())
    }

    /// Close the host stream of `stream_id`, if open.
    fn close_host(&mut self, stream_id: usize) {
        self.streams[stream_id].host = None;
    }

    /// Serve one control request, returning the response bytes (the
    /// status word first, as §5.14.6 requires of every response).
    fn control(&mut self, request: &[u8]) -> Vec<u8> {
        let Some(code) = request.first_chunk::<4>().map(|b| u32::from_le_bytes(*b)) else {
            return status(S_BAD_MSG);
        };
        match code {
            R_PCM_INFO => self.pcm_info(request),
            R_PCM_SET_PARAMS => self.set_params(request),
            R_PCM_PREPARE | R_PCM_START | R_PCM_STOP | R_PCM_RELEASE => {
                self.pcm_command(code, request)
            }
            // No jacks, no channel maps, no control elements: any other
            // request names an item this device does not have.
            _ => status(S_NOT_SUPP),
        }
    }

    /// `VIRTIO_SND_R_PCM_INFO`: status plus one `virtio_snd_pcm_info` per
    /// requested stream, truncated or zero-padded to the guest's `size`.
    fn pcm_info(&self, request: &[u8]) -> Vec<u8> {
        let out_of_range = || status(S_BAD_MSG);
        let Some(body) = request.get(4..16) else {
            return out_of_range();
        };
        let start_id = u32::from_le_bytes(body[0..4].try_into().unwrap());
        let count = u32::from_le_bytes(body[4..8].try_into().unwrap());
        let size = u32::from_le_bytes(body[8..12].try_into().unwrap()) as usize;
        if size == 0
            || start_id > self.streams.len() as u32
            || count > self.streams.len() as u32 - start_id
        {
            return out_of_range();
        }
        let mut out = status(S_OK);
        for i in 0..count as usize {
            let index = start_id as usize + i;
            let mut info = [0u8; PCM_INFO_LEN];
            // hda_fn_nid 0: nothing to link without jacks or channel maps.
            info[4..8].copy_from_slice(&0u32.to_le_bytes()); // features: none
            let formats = (1u64 << PCM_FMT_S16) | (1u64 << PCM_FMT_FLOAT);
            info[8..16].copy_from_slice(&formats.to_le_bytes());
            let rates =
                (1u64 << PCM_RATE_44100) | (1u64 << PCM_RATE_48000) | (1u64 << PCM_RATE_96000);
            info[16..24].copy_from_slice(&rates.to_le_bytes());
            info[24] = u8::from(index != 0); // direction
            info[25] = 1; // channels_min
            info[26] = 2; // channels_max
            out.extend_from_slice(&info[..size.min(PCM_INFO_LEN)]);
            if size > PCM_INFO_LEN {
                out.resize(out.len() + size - PCM_INFO_LEN, 0);
            }
        }
        out
    }

    /// `VIRTIO_SND_R_PCM_SET_PARAMS` (§5.14.6.6.3).
    fn set_params(&mut self, request: &[u8]) -> Vec<u8> {
        let bad_msg = || status(S_BAD_MSG);
        let Some(body) = request.get(4..24) else {
            return bad_msg();
        };
        let stream_id = u32::from_le_bytes(body[0..4].try_into().unwrap());
        let buffer_bytes = u32::from_le_bytes(body[4..8].try_into().unwrap());
        let period_bytes = u32::from_le_bytes(body[8..12].try_into().unwrap());
        let features = u32::from_le_bytes(body[12..16].try_into().unwrap());
        let channels = body[16];
        let format = body[17];
        let rate = body[18];
        if !self.stream_ok(stream_id) {
            return bad_msg();
        }
        // Re-negotiating a started stream restarts the lifecycle there:
        // the underrun-recovery path of at least one mainline kernel
        // re-sends parameters before stopping. The device accepts and
        // stops, as a stopped stream accepts them too.
        let stream = &mut self.streams[stream_id as usize];

        // The stream information offers no PCM features; none may be picked.
        if features != 0 {
            return status(S_NOT_SUPP);
        }
        let sample_format = match format {
            PCM_FMT_S16 => SampleFormat::S16Le,
            PCM_FMT_FLOAT => SampleFormat::F32Le,
            _ => return status(S_NOT_SUPP),
        };
        let rate_hz = match rate {
            PCM_RATE_44100 => 44100,
            PCM_RATE_48000 => 48000,
            PCM_RATE_96000 => 96000,
            _ => return status(S_NOT_SUPP),
        };
        if !(1..=2).contains(&channels) {
            return status(S_NOT_SUPP);
        }
        let format = AudioFormat {
            sample_format,
            channels,
            rate: rate_hz,
        };
        if buffer_bytes == 0
            || period_bytes == 0
            || buffer_bytes % period_bytes != 0
            || period_bytes % format.frame_bytes() as u32 != 0
        {
            return bad_msg();
        }
        // New parameters restart the lifecycle: any host stream opened for
        // the old ones is closed (§5.14.6.6.1).
        stream.host = None;
        stream.state = PcmState::Set;
        stream.due_frames = 0;
        stream.fraction = 0;
        stream.tx_credit = 0;
        stream.params = Some(PcmParams {
            buffer_bytes,
            period_bytes,
            features,
            format,
            rate_code: rate,
        });
        status(S_OK)
    }

    /// PREPARE, START, STOP and RELEASE (§5.14.6.6.1, §5.14.6.6.4-7).
    fn pcm_command(&mut self, code: u32, request: &[u8]) -> Vec<u8> {
        let bad_msg = || status(S_BAD_MSG);
        let Some(body) = request.get(4..8) else {
            return bad_msg();
        };
        let stream_id = u32::from_le_bytes(body[0..4].try_into().unwrap());
        if !self.stream_ok(stream_id) {
            return bad_msg();
        }
        let index = stream_id as usize;
        // RELEASE also answers from Set: the Linux driver frees hardware
        // parameters it never prepared, with messages already allocated.
        // The lifecycle table (§5.14.6.6.1) bounds what a well driver may
        // send; an underrun recovery re-runs prepare, start and stop in
        // whatever state the xrun left the substream in, so this device
        // answers the idempotent form instead of BAD_MSG and lets the
        // guest converge. Nothing the spec makes the device check is
        // weakened: a message never touches an unprepared stream.
        let allowed = match code {
            R_PCM_PREPARE => {
                self.streams[index].params.is_some()
                    && !matches!(self.streams[index].state, PcmState::Unused)
            }
            R_PCM_START => matches!(
                self.streams[index].state,
                PcmState::Prepared | PcmState::Stopped | PcmState::Started
            ),
            R_PCM_STOP => matches!(
                self.streams[index].state,
                PcmState::Started | PcmState::Stopped
            ),
            R_PCM_RELEASE => matches!(
                self.streams[index].state,
                PcmState::Set | PcmState::Prepared | PcmState::Stopped | PcmState::Released
            ),
            _ => return bad_msg(),
        };
        if !allowed {
            return bad_msg();
        }
        match code {
            R_PCM_PREPARE => {
                // Re-preparing a started stream restarts it where it
                // stands: the host stream reopens, the pacing resets.
                self.close_host(index);
                match self.open_host(index, false) {
                    Ok(()) => {}
                    Err(error) => {
                        eprintln!("vmlab-hv: virtio-snd host stream: {error}");
                        return status(S_IO_ERR);
                    }
                }
                let stream = &mut self.streams[index];
                stream.due_frames = 0;
                stream.fraction = 0;
                stream.tx_credit = 0;
                stream.state = PcmState::Prepared;
            }
            R_PCM_START => self.streams[index].state = PcmState::Started,
            R_PCM_STOP => {
                // Messages already in the queue keep their places: the
                // guest may resume them with START.
                self.streams[index].state = PcmState::Stopped;
            }
            R_PCM_RELEASE => {
                // §5.14.6.6.5.1: every pending I/O message for this stream
                // completes before the control request does. The stream is
                // not started, so nothing its queue still holds was ever
                // consumed; the messages complete as cancelled.
                self.complete_pending(index);
                self.close_host(index);
                self.streams[index].state = PcmState::Released;
            }
            _ => unreachable!("dispatch checked the code"),
        }
        status(S_OK)
    }

    /// Complete every message waiting on `stream_id`'s data queue with
    /// `VIRTIO_SND_S_IO_ERR`; used by RELEASE.
    fn complete_pending(&mut self, stream_id: usize) {
        if stream_id == 0 {
            self.drain_tx(true);
        } else {
            self.drain_rx(true);
        }
    }

    /// Drain the tx queue of the playback stream (§5.14.6.8.1). Paced by
    /// the host sink: a message is taken only while the sink's queued PCM
    /// is below the watermark, so the guest's own `buffer_bytes` bounds
    /// the data in flight and completions follow real playback. With
    /// `all`, everything waiting completes as cancelled (RELEASE), which
    /// a started stream never reaches.
    fn drain_tx(&mut self, all: bool) {
        let Some(mem) = self.mem.clone() else { return };
        let Some(interrupt) = self.interrupt.clone() else {
            return;
        };
        if !self.queues.get(VQ_TX).is_some_and(|q| q.ready()) {
            return;
        }
        let started = self
            .streams
            .first()
            .is_some_and(|s| s.state == PcmState::Started && s.host.is_some());
        let params = self.streams.first().and_then(|s| s.params);
        let target = params.map_or(0, |p| {
            queued_target(p.period_bytes as usize, p.format.frame_bytes())
        });
        let period_frames = params.map_or(0, |p| p.period_frames()) as u64;
        let mut used_any = false;
        let mut budget = TX_FRAMES_PER_TICK;
        loop {
            if !all {
                if !started || budget == 0 {
                    break;
                }
                // A period of machine time must be banked before the next
                // message may play: completions are elapsed-period
                // notifications, never faster than real time.
                if self
                    .streams
                    .first()
                    .is_none_or(|s| s.tx_credit < period_frames)
                {
                    break;
                }
                let queued = match self.streams.first().and_then(|s| s.host.as_ref()) {
                    Some(HostStream::Playback(sink)) => sink.queued_bytes(),
                    _ => break, // no host stream: hold the queue
                };
                if queued >= target {
                    break; // paced: the rest waits for the host
                }
            }
            let Some(chain) = self
                .queues
                .get_mut(VQ_TX)
                .and_then(|q| q.pop_descriptor_chain(&mem))
            else {
                break;
            };
            let head = chain.head_index();
            let mut reader = match chain.clone().reader(&mem) {
                Ok(reader) => reader,
                Err(_) => {
                    let _ = self
                        .queues
                        .get_mut(VQ_TX)
                        .map(|q| q.add_used(&mem, head, 0));
                    used_any = true;
                    continue;
                }
            };
            // The xfer header names the target stream. A message for any
            // other stream, or with no readable PCM, completes cancelled.
            let mut header = [0u8; 4];
            let header_ok = reader.read_exact(&mut header).is_ok();
            let mut pcm = Vec::new();
            let pcm_ok = reader.read_to_end(&mut pcm).is_ok();
            let deliver = !all
                && header_ok
                && pcm_ok
                && started
                && params.is_some()
                && u32::from_le_bytes(header) == 0;
            let (code, latency) = if deliver {
                let frame_bytes = params.unwrap().format.frame_bytes();
                let mut outcome = (S_IO_ERR, 0);
                if let Some(stream) = self.streams.first_mut()
                    && let Some(HostStream::Playback(sink)) = stream.host.as_mut()
                {
                    for part in pcm.chunks(PUSH_FRAMES * frame_bytes) {
                        sink.write_period(part);
                    }
                    let frames = (pcm.len() / frame_bytes) as u64;
                    budget = budget.saturating_sub(frames as usize);
                    // Only delivered audio spends the credit; a cancelled
                    // message never reaches the speaker.
                    stream.tx_credit = stream.tx_credit.saturating_sub(frames);
                    outcome = (S_OK, sink.latency_bytes());
                }
                outcome
            } else {
                (S_IO_ERR, 0)
            };
            let written = complete_message(&mem, &chain, code, latency, None);
            let _ = self
                .queues
                .get_mut(VQ_TX)
                .map(|q| q.add_used(&mem, head, written as u32));
            used_any = true;
            // With VIRTIO_F_RING_EVENT_IDX negotiated, the drain re-arms
            // notifications and catches messages that arrived while the
            // response was being published.
            let en = self
                .queues
                .get_mut(VQ_TX)
                .map(|q| q.enable_notification(&mem));
            match en.map(|r| r.ok()) {
                Some(Some(true)) if !all => continue,
                _ => break,
            }
        }
        let want = used_any
            && self
                .queues
                .get_mut(VQ_TX)
                .is_some_and(|q| q.needs_notification(&mem).unwrap_or(false));
        if want {
            interrupt.notify_queue(VQ_TX as u16);
        }
    }

    /// Drain the rx queue of the capture stream (§5.14.6.8.2). Paced by
    /// banked machine time: one message is filled with one pull from the
    /// host source once `due_frames` covers a period, keeping consumption
    /// equal to real time. With `all`, everything waiting completes as
    /// cancelled (RELEASE).
    fn drain_rx(&mut self, all: bool) {
        let Some(mem) = self.mem.clone() else { return };
        let Some(interrupt) = self.interrupt.clone() else {
            return;
        };
        if !self.queues.get(VQ_RX).is_some_and(|q| q.ready()) {
            return;
        }
        // Without a capture stream (microphone off) no rx message can be
        // legitimate, so every one completes as cancelled immediately.
        let capture_exists = self.streams.len() >= 2;
        let mut used_any = false;
        loop {
            if !all && capture_exists {
                let Some(stream) = self.streams.get(1) else {
                    break;
                };
                let Some(params) = stream.params else { break };
                if stream.state != PcmState::Started
                    || stream.host.is_none()
                    || (stream.due_frames as usize)
                        < params.period_bytes as usize / params.format.frame_bytes()
                {
                    break;
                }
            }
            let Some(chain) = self
                .queues
                .get_mut(VQ_RX)
                .and_then(|q| q.pop_descriptor_chain(&mem))
            else {
                break;
            };
            let head = chain.head_index();
            let mut reader = match chain.clone().reader(&mem) {
                Ok(reader) => reader,
                Err(_) => {
                    let _ = self
                        .queues
                        .get_mut(VQ_RX)
                        .map(|q| q.add_used(&mem, head, 0));
                    used_any = true;
                    continue;
                }
            };
            let mut header = [0u8; 4];
            let header_ok = reader.read_exact(&mut header).is_ok();
            let mut writer = match chain.clone().writer(&mem) {
                Ok(writer) => writer,
                Err(_) => {
                    let _ = self
                        .queues
                        .get_mut(VQ_RX)
                        .map(|q| q.add_used(&mem, head, 0));
                    used_any = true;
                    continue;
                }
            };
            if writer.available_bytes() < 8 {
                // No room for the status part: nothing to answer on.
                let _ = self
                    .queues
                    .get_mut(VQ_RX)
                    .map(|q| q.add_used(&mem, head, 0));
                used_any = true;
                continue;
            }
            let capacity = writer.available_bytes() - 8;
            let started = capture_exists
                && self.streams[1].state == PcmState::Started
                && self.streams[1].host.is_some();
            let fill = if !started || all || !header_ok || u32::from_le_bytes(header) != 1 {
                0
            } else if let Some(params) = self.streams[1].params {
                let frame_bytes = params.format.frame_bytes();
                let frames = capacity / frame_bytes;
                let mut pcm = vec![0u8; frames * frame_bytes];
                match self.streams[1].host.as_mut() {
                    Some(HostStream::Capture(source)) => {
                        source.read_period(&mut pcm);
                        let _ = writer.write_all(&pcm);
                        self.streams[1].due_frames =
                            self.streams[1].due_frames.saturating_sub(frames as u64);
                        frames * frame_bytes
                    }
                    _ => 0,
                }
            } else {
                // Unreachable through the pacing gate: a started capture
                // stream always carries parameters.
                0
            };
            let (code, latency) = if fill > 0 {
                let latency = match self.streams[1].host.as_ref() {
                    Some(HostStream::Capture(source)) => source.latency_bytes(),
                    _ => 0,
                };
                (S_OK, latency)
            } else {
                (S_IO_ERR, 0)
            };
            let written = complete_message(&mem, &chain, code, latency, Some(fill as u32));
            let _ = self
                .queues
                .get_mut(VQ_RX)
                .map(|q| q.add_used(&mem, head, written as u32));
            used_any = true;
            if !all
                && self
                    .queues
                    .get_mut(VQ_RX)
                    .map(|q| q.enable_notification(&mem).ok())
                    != Some(Some(true))
            {
                break;
            }
        }
        if used_any
            && self
                .queues
                .get_mut(VQ_RX)
                .is_some_and(|q| q.needs_notification(&mem).unwrap_or(false))
        {
            interrupt.notify_queue(VQ_RX as u16);
        }
    }

    /// Drain the control queue: one request-and-response chain at a time.
    fn drain_control(&mut self) {
        let Some(mem) = self.mem.clone() else { return };
        let Some(interrupt) = self.interrupt.clone() else {
            return;
        };
        if !self.queues.get(VQ_CONTROL).is_some_and(|q| q.ready()) {
            return;
        }
        let mut used_any = false;
        while let Some(chain) = self
            .queues
            .get_mut(VQ_CONTROL)
            .and_then(|q| q.pop_descriptor_chain(&mem))
        {
            let head = chain.head_index();
            let response = match chain.clone().reader(&mem) {
                Ok(mut reader) => {
                    let mut request = Vec::new();
                    reader.read_to_end(&mut request).ok();
                    self.control(&request)
                }
                Err(_) => status(S_BAD_MSG),
            };
            let written = match chain.clone().writer(&mem) {
                Ok(mut writer) => {
                    writer.write_all(&response).ok();
                    writer.bytes_written() as u32
                }
                Err(_) => 0,
            };
            let _ = self
                .queues
                .get_mut(VQ_CONTROL)
                .map(|q| q.add_used(&mem, head, written));
            used_any = true;
            // More requests may have arrived while the response was being
            // published (VIRTIO_F_RING_EVENT_IDX suppression).
            if self
                .queues
                .get_mut(VQ_CONTROL)
                .map(|q| q.enable_notification(&mem).ok())
                != Some(Some(true))
            {
                break;
            }
        }
        if used_any
            && self
                .queues
                .get_mut(VQ_CONTROL)
                .is_some_and(|q| q.needs_notification(&mem).unwrap_or(false))
        {
            interrupt.notify_queue(VQ_CONTROL as u16);
        }
    }
}

fn status(code: u32) -> Vec<u8> {
    code.to_le_bytes().to_vec()
}

/// Write the `virtio_snd_pcm_status` tail of an I/O message and return
/// the byte count for the used ring: the status alone (tx), or the
/// recorded frames plus it (rx). The status part is the message's last
/// buffer, so it lands at the end of the chain's writable descriptors.
fn complete_message(
    mem: &GuestMemory,
    chain: &virtio_queue::DescriptorChain<&GuestMemory>,
    code: u32,
    latency_bytes: usize,
    recorded: Option<u32>,
) -> usize {
    let mut writer = match chain.clone().writer(mem) {
        Ok(writer) => writer,
        Err(_) => return 0,
    };
    let mut payload = [0u8; 8];
    payload[0..4].copy_from_slice(&code.to_le_bytes());
    payload[4..8].copy_from_slice(&(latency_bytes as u32).to_le_bytes());
    let available = writer.available_bytes();
    if available >= 8 {
        // The last 8 writable bytes are the status buffer. A tx chain's
        // only writable buffer is exactly that (split_at(0) would yield an
        // empty writer); an rx chain's writer covers the recorded frames
        // first, so the status is split off the end.
        let written = if available == 8 {
            writer.write_all(&payload).is_ok()
        } else {
            match writer.split_at(available - 8) {
                Ok(mut tail) => tail.write_all(&payload).is_ok(),
                Err(_) => false,
            }
        };
        if written {
            return 8 + recorded.unwrap_or(0) as usize;
        }
    }
    writer.bytes_written()
}

/// The machine-clock side of the device. The capture stream banks the
/// frames it is owed, then both data queues drain under their pacing
/// rules; the tx side needs no time of its own, because the host sink's
/// queue depth is its clock.
impl AudioClock for VirtioSnd {
    fn advance(&mut self, elapsed: Duration) -> Result<(), DeviceError> {
        if self.paused {
            return Ok(());
        }
        for stream in &mut self.streams {
            if stream.state != PcmState::Started || stream.params.is_none() {
                continue;
            }
            let rate = stream.params.unwrap().format.rate;
            let frames = elapsed.as_nanos() * u128::from(rate) + u128::from(stream.fraction);
            stream.fraction = (frames % 1_000_000_000) as u64;
            let frames = (frames / 1_000_000_000) as u64;
            // Capture banks what it is owed; playback banks what it may
            // deliver. Both accrue at the stream's frame rate, so guest
            // data never moves faster than real time in either direction.
            if stream
                .host
                .as_ref()
                .is_some_and(|h| matches!(h, HostStream::Capture(_)))
            {
                stream.due_frames += frames;
            } else {
                stream.tx_credit += frames;
            }
        }
        self.drain_tx(false);
        self.drain_rx(false);
        Ok(())
    }

    /// Freeze guest-visible progress: no completions, no capture pulls.
    fn set_paused(&mut self, paused: bool) {
        self.paused = paused;
    }
}

impl VirtioDevice for VirtioSnd {
    fn device_type(&self) -> u32 {
        VIRTIO_ID_SOUND
    }

    fn queue_max_sizes(&self) -> &[u16] {
        &self.queue_sizes
    }

    fn features(&self) -> u64 {
        self.features
    }

    fn ack_features(&mut self, features: u64) {
        self.acked_features = features & self.features;
    }

    fn config_len(&self) -> u64 {
        CONFIG_LEN as u64
    }

    fn read_config(&self, offset: u64, data: &mut [u8]) {
        if offset >= CONFIG_LEN as u64 {
            data.fill(0);
            return;
        }
        let start = offset as usize;
        let end = (start + data.len()).min(CONFIG_LEN);
        data[..end - start].copy_from_slice(&self.config[start..end]);
        data[end - start..].fill(0);
    }

    fn write_config(&mut self, _offset: u64, _data: &[u8]) {
        // The configuration is read-only for the driver.
    }

    fn activate(
        &mut self,
        mem: GuestMemory,
        interrupt: Arc<dyn VirtioInterrupt>,
        mut queues: Vec<Queue>,
    ) -> Result<(), DeviceError> {
        if let Some(states) = self.pending_queues.take() {
            for (queue, state) in queues.iter_mut().zip(states) {
                if let Ok(restored) = Queue::try_from(state) {
                    *queue = restored;
                }
            }
        }
        self.mem = Some(mem);
        self.interrupt = Some(interrupt);
        self.queues = queues;
        Ok(())
    }

    fn queue_notify(&mut self, index: u16) {
        match index as usize {
            VQ_CONTROL => self.drain_control(),
            VQ_TX => self.drain_tx(false),
            VQ_RX => self.drain_rx(false),
            _ => {} // the event queue never carries device-written buffers
        }
    }

    fn reset(&mut self) {
        self.acked_features = 0;
        self.mem = None;
        self.interrupt = None;
        self.queues.clear();
        self.pending_queues = None;
        for stream in &mut self.streams {
            *stream = Stream::default();
        }
        self.paused = false;
    }
}

/// Bytes per queue state, matching the block device's wire layout:
/// max_size, next_avail, next_used, event_idx, size, ready, then the
/// three 64-bit ring addresses.
const QUEUE_STATE_BYTES: usize = 34;

/// Bytes per stream in the snapshot: state, buffer and period bytes,
/// features, channels, format, rate, padding, then the two per-direction
/// time accumulators (capture owed, playback credit) and the shared
/// sub-frame remainder.
const STREAM_STATE_BYTES: usize = 41;

const SNAPSHOT_MAGIC: u32 = u32::from_le_bytes(*b"VSND");

/// Snapshot layout: magic, version, acked features, queue count, one
/// queue-state blob per queue, stream count, then per stream: state byte,
/// parameters and the capture time accumulator. Host streams are not
/// saved; restore reopens them and discards one period of capture (the
/// audio plan's snapshot rule: at most one period glitches). In-flight
/// buffers need no section of their own: a message is only ever completed
/// whole, so the un-consumed ones are exactly the queue states saved
/// here.
impl Snapshot for VirtioSnd {
    fn state_id(&self) -> String {
        self.id.clone()
    }

    fn save_state(&self) -> Result<Vec<u8>, DeviceError> {
        let mut out = Vec::with_capacity(
            15 + NUM_QUEUES * QUEUE_STATE_BYTES + self.streams.len() * STREAM_STATE_BYTES,
        );
        out.extend_from_slice(&SNAPSHOT_MAGIC.to_le_bytes());
        out.push(1); // format version
        out.extend_from_slice(&self.acked_features.to_le_bytes());
        out.extend_from_slice(&(self.queues.len() as u16).to_le_bytes());
        for q in &self.queues {
            let s = q.state();
            out.extend_from_slice(&s.max_size.to_le_bytes());
            out.extend_from_slice(&s.next_avail.to_le_bytes());
            out.extend_from_slice(&s.next_used.to_le_bytes());
            out.extend_from_slice(&(s.event_idx_enabled as u8).to_le_bytes());
            out.extend_from_slice(&s.size.to_le_bytes());
            out.extend_from_slice(&(s.ready as u8).to_le_bytes());
            out.extend_from_slice(&s.desc_table.to_le_bytes());
            out.extend_from_slice(&s.avail_ring.to_le_bytes());
            out.extend_from_slice(&s.used_ring.to_le_bytes());
        }
        out.push(self.streams.len() as u8);
        for stream in &self.streams {
            out.push(match stream.state {
                PcmState::Unused | PcmState::Released if stream.params.is_none() => 0,
                PcmState::Unused | PcmState::Set => 1,
                PcmState::Prepared => 2,
                PcmState::Started => 3,
                PcmState::Stopped => 4,
                PcmState::Released => 5,
            });
            let (buffer_bytes, period_bytes, features, channels, format, rate) = match stream.params
            {
                Some(p) => (
                    p.buffer_bytes,
                    p.period_bytes,
                    p.features,
                    p.format.channels,
                    match p.format.sample_format {
                        SampleFormat::S16Le => PCM_FMT_S16,
                        SampleFormat::F32Le => PCM_FMT_FLOAT,
                    },
                    p.rate_code,
                ),
                None => (0, 0, 0, 0, PCM_FMT_S16, PCM_RATE_48000),
            };
            out.extend_from_slice(&buffer_bytes.to_le_bytes());
            out.extend_from_slice(&period_bytes.to_le_bytes());
            out.extend_from_slice(&features.to_le_bytes());
            out.push(channels);
            out.push(format);
            out.push(rate);
            out.push(0); // padding
            out.extend_from_slice(&stream.due_frames.to_le_bytes());
            out.extend_from_slice(&stream.tx_credit.to_le_bytes());
            out.extend_from_slice(&stream.fraction.to_le_bytes());
        }
        Ok(out)
    }

    fn restore_state(&mut self, state: &[u8]) -> Result<(), DeviceError> {
        let id = self.id.clone();
        let invalid = move |what: &str| DeviceError::State(format!("{id}: {what}"));
        if state.len() < 15
            || u32::from_le_bytes(state[0..4].try_into().unwrap()) != SNAPSHOT_MAGIC
            || state[4] != 1
        {
            return Err(invalid("truncated sound state"));
        }
        let acked = u64::from_le_bytes(state[5..13].try_into().unwrap());
        let queue_count = u16::from_le_bytes(state[13..15].try_into().unwrap()) as usize;
        let mut at = 15 + queue_count * QUEUE_STATE_BYTES;
        if queue_count != NUM_QUEUES || state.len() <= at {
            return Err(invalid("bad sound state"));
        }
        let stream_count = state[at] as usize;
        at += 1;
        if stream_count != self.streams.len()
            || state.len() != at + stream_count * STREAM_STATE_BYTES
        {
            return Err(invalid("bad sound state"));
        }
        let mut states = Vec::with_capacity(queue_count);
        for i in 0..queue_count {
            let b = &state[15 + i * QUEUE_STATE_BYTES..];
            let u16at = |k: usize| -> Result<u16, DeviceError> {
                b[k..k + 2]
                    .try_into()
                    .map(u16::from_le_bytes)
                    .map_err(|_| invalid("bad sound state"))
            };
            let u64at = |k: usize| -> Result<u64, DeviceError> {
                b[k..k + 8]
                    .try_into()
                    .map(u64::from_le_bytes)
                    .map_err(|_| invalid("bad sound state"))
            };
            states.push(QueueState {
                max_size: u16at(0)?,
                next_avail: u16at(2)?,
                next_used: u16at(4)?,
                event_idx_enabled: b[6] != 0,
                size: u16at(7)?,
                ready: b[9] != 0,
                desc_table: u64at(10)?,
                avail_ring: u64at(18)?,
                used_ring: u64at(26)?,
            });
        }
        self.acked_features = acked & self.features;
        if self.mem.is_some() {
            for (q, s) in self.queues.iter_mut().zip(states) {
                *q = Queue::try_from(s).map_err(|_| invalid("bad sound state"))?;
            }
        } else {
            self.pending_queues = Some(states);
        }
        for i in 0..self.streams.len() {
            let stream = &mut self.streams[i];
            let b = &state[at + i * STREAM_STATE_BYTES..];
            let params = if b[13] > 0 {
                let sample_format = match b[14] {
                    PCM_FMT_S16 => SampleFormat::S16Le,
                    PCM_FMT_FLOAT => SampleFormat::F32Le,
                    _ => return Err(invalid("bad sound state")),
                };
                let rate = match b[15] {
                    PCM_RATE_44100 => 44100,
                    PCM_RATE_48000 => 48000,
                    PCM_RATE_96000 => 96000,
                    _ => return Err(invalid("bad sound state")),
                };
                Some(PcmParams {
                    buffer_bytes: u32::from_le_bytes(b[1..5].try_into().unwrap()),
                    period_bytes: u32::from_le_bytes(b[5..9].try_into().unwrap()),
                    features: u32::from_le_bytes(b[9..13].try_into().unwrap()),
                    format: AudioFormat {
                        sample_format,
                        channels: b[13],
                        rate,
                    },
                    rate_code: b[15],
                })
            } else {
                None
            };
            stream.state = match b[0] {
                0 => PcmState::Unused,
                1 => PcmState::Set,
                2 => PcmState::Prepared,
                3 => PcmState::Started,
                4 => PcmState::Stopped,
                5 => PcmState::Released,
                _ => return Err(invalid("bad sound state")),
            };
            stream.params = params;
            stream.host = None;
            stream.due_frames = u64::from_le_bytes(b[17..25].try_into().unwrap());
            stream.fraction = u64::from_le_bytes(b[25..33].try_into().unwrap());
            // A stream that was past PREPARE reopens its host stream. The
            // one discarded capture period is the audio plan's snapshot
            // rule: the host buffered it before the machine froze.
            if matches!(
                stream.state,
                PcmState::Prepared | PcmState::Started | PcmState::Stopped
            ) && stream.params.is_some()
            {
                self.open_host(i, true)
                    .map_err(|_| invalid("bad sound state"))?;
            }
        }
        Ok(())
    }
}

/// The transport-facing handle of a [`VirtioSnd`] it shares with the
/// machine clock. Every call locks the shared device; the lock order is
/// always PCI root → transport → device, and the timer path takes the
/// device alone, so the two never deadlock.
pub struct SoundPci {
    device: Arc<Mutex<VirtioSnd>>,
    /// The queue sizes, copied at construction: [`VirtioDevice`] lends
    /// them as a slice, which a lock guard cannot outlive.
    queue_sizes: Vec<u16>,
}

impl VirtioDevice for SoundPci {
    fn device_type(&self) -> u32 {
        self.device.lock().device_type()
    }
    fn queue_max_sizes(&self) -> &[u16] {
        &self.queue_sizes
    }
    fn features(&self) -> u64 {
        self.device.lock().features()
    }
    fn ack_features(&mut self, features: u64) {
        self.device.lock().ack_features(features);
    }
    fn read_config(&self, offset: u64, data: &mut [u8]) {
        self.device.lock().read_config(offset, data);
    }
    fn write_config(&mut self, offset: u64, data: &[u8]) {
        self.device.lock().write_config(offset, data);
    }
    fn config_len(&self) -> u64 {
        self.device.lock().config_len()
    }
    fn activate(
        &mut self,
        mem: GuestMemory,
        interrupt: Arc<dyn VirtioInterrupt>,
        queues: Vec<Queue>,
    ) -> Result<(), DeviceError> {
        self.device.lock().activate(mem, interrupt, queues)
    }
    fn queue_notify(&mut self, index: u16) {
        self.device.lock().queue_notify(index);
    }
    fn reset(&mut self) {
        self.device.lock().reset();
    }
}

impl Snapshot for SoundPci {
    fn state_id(&self) -> String {
        self.device.lock().state_id()
    }
    fn save_state(&self) -> Result<Vec<u8>, DeviceError> {
        self.device.lock().save_state()
    }
    fn restore_state(&mut self, state: &[u8]) -> Result<(), DeviceError> {
        self.device.lock().restore_state(state)
    }
}
