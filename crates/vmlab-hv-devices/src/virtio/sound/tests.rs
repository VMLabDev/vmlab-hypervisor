//! A2 (hv-audio SPEC, "Testing decisions"): virtio-snd driven through the
//! virtio-pci transport, exactly the way a driver sees it — PCI discovery,
//! common-configuration queue setup, control messages and PCM messages on
//! the tx and rx queues — with playback landing in the recording backend
//! and capture served from a script. The pacing cases use a host double
//! whose queue depth and latency the test controls, because completions
//! must follow host consumption, not the guest's pace.

use std::sync::Arc;
use std::time::Duration;

use parking_lot::Mutex;
use vm_memory::{Bytes, GuestAddress, GuestMemoryMmap, Le16, Le32};

use super::VirtioSnd;
use crate::Snapshot;
use crate::audio::recording::RecordingHost;
use crate::audio::{AudioClock, CaptureSource, HostAudio, PlaybackSink};
use crate::virtio::pci::VirtioPciDevice;
use crate::virtio::sound::SoundPci;
use crate::virtio::testing::{RecIrq, RecMsi};
use crate::{DeviceError, PciDevice, VirtioDevice};

// Driver-side constants mirrored from the transport's BAR layout
// (VIRTIO 1.2 §4.1.4-4.1.5): one 64-bit BAR carries everything.
const COMMON_CFG: u64 = 0x0000;
const NOTIFY: u64 = 0x1000;
const DEVICE_CFG: u64 = 0x3000;
const MSIX_TABLE: u64 = 0x4000;

const QUEUE_SELECT: u64 = COMMON_CFG + 0x16;
const QUEUE_SIZE: u64 = COMMON_CFG + 0x18;
const QUEUE_MSIX_VECTOR: u64 = COMMON_CFG + 0x1A;
const QUEUE_ENABLE: u64 = COMMON_CFG + 0x1C;
const QUEUE_DESC: u64 = COMMON_CFG + 0x20;
const QUEUE_AVAIL: u64 = COMMON_CFG + 0x28;
const QUEUE_USED: u64 = COMMON_CFG + 0x30;
const NUM_QUEUES: u64 = COMMON_CFG + 0x12;
const DEVICE_STATUS: u64 = COMMON_CFG + 0x14;
const GUEST_FEATURE_SELECT: u64 = COMMON_CFG + 0x08;
const GUEST_FEATURE: u64 = COMMON_CFG + 0x0C;

/// Descriptor flags (VIRTIO 1.2 §2.6.5).
const NEXT: u16 = 1;
const WRITE: u16 = 2;

const VQ_CONTROL: u16 = 0;
const VQ_TX: u16 = 2;
const VQ_RX: u16 = 3;
const SIZE: u16 = 64;

// Guest memory map: four descriptor/avail/used ring sets, then control,
// tx and rx message buffers.
fn rings(queue: u16) -> (u64, u64, u64) {
    let base = 0x20000 + u64::from(queue) * 0x8000;
    (base, base + 0x1000, base + 0x2000)
}
const CTRL_REQ: u64 = 0x40000;
const CTRL_RESP: u64 = 0x42000;
const TX_BASE: u64 = 0x50000;
const RX_BASE: u64 = 0x60000;

/// One control message code per name, for readable request builders.
const R_PCM_INFO: u32 = 0x0100;
const R_PCM_SET_PARAMS: u32 = 0x0101;
const R_PCM_PREPARE: u32 = 0x0102;
const R_PCM_RELEASE: u32 = 0x0103;
const R_PCM_START: u32 = 0x0104;
const R_PCM_STOP: u32 = 0x0105;
const R_CTL_INFO: u32 = 0x0300;
const R_JACK_INFO: u32 = 1;

const S_OK: u32 = 0x8000;
const S_BAD_MSG: u32 = 0x8001;
const S_NOT_SUPP: u32 = 0x8002;
const S_IO_ERR: u32 = 0x8003;

const PCM_FMT_S16: u8 = 5;
const PCM_FMT_FLOAT: u8 = 19;
const PCM_RATE_44100: u8 = 6;
const PCM_RATE_48000: u8 = 7;
const PCM_RATE_96000: u8 = 10;

/// A host the test paces: playback lands in `pcm`, `queued` only shrinks
/// when the test says the host consumed, and `latency_bytes` reports the
/// queue plus a constant graph offset so the two are distinguishable.
/// Capture serves a script, one entry per pull, then silence.
#[derive(Default)]
struct PacedState {
    playback_opens: usize,
    capture_opens: usize,
    pcm: Vec<u8>,
    queued: usize,
    script: std::collections::VecDeque<Vec<u8>>,
    pulls: Vec<usize>,
}

#[derive(Clone, Default)]
struct PacedHost(Arc<Mutex<PacedState>>);

impl PacedHost {
    fn with_script(script: Vec<Vec<u8>>) -> Self {
        let host = Self::default();
        host.0.lock().script = script.into();
        host
    }

    /// The host consumed `bytes` of what the device pushed.
    fn drain(&self, bytes: usize) {
        let mut state = self.0.lock();
        state.queued = state.queued.saturating_sub(bytes);
    }

    fn pcm(&self) -> Vec<u8> {
        self.0.lock().pcm.clone()
    }

    fn playback_opens(&self) -> usize {
        self.0.lock().playback_opens
    }

    fn capture_opens(&self) -> usize {
        self.0.lock().capture_opens
    }

    /// The sizes, in order, of every capture pull (script or silence).
    fn pulls(&self) -> Vec<usize> {
        self.0.lock().pulls.clone()
    }
}

struct PacedSink(Arc<Mutex<PacedState>>);

impl PlaybackSink for PacedSink {
    fn write_period(&mut self, pcm: &[u8]) {
        let mut state = self.0.lock();
        state.pcm.extend_from_slice(pcm);
        state.queued += pcm.len();
    }
    fn latency_bytes(&self) -> usize {
        // The graph offset proves the device reports latency, not queue
        // depth, in virtio_snd_pcm_status.
        self.0.lock().queued + 123
    }
    fn queued_bytes(&self) -> usize {
        self.0.lock().queued
    }
}

struct PacedSource(Arc<Mutex<PacedState>>);

impl CaptureSource for PacedSource {
    fn read_period(&mut self, out: &mut [u8]) {
        let mut state = self.0.lock();
        state.pulls.push(out.len());
        match state.script.pop_front() {
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

impl HostAudio for PacedHost {
    fn name(&self) -> &'static str {
        "paced"
    }
    fn open_playback(
        &self,
        _: crate::audio::AudioFormat,
    ) -> Result<Box<dyn PlaybackSink>, DeviceError> {
        self.0.lock().playback_opens += 1;
        Ok(Box::new(PacedSink(self.0.clone())))
    }
    fn open_capture(
        &self,
        _: crate::audio::AudioFormat,
    ) -> Result<Box<dyn CaptureSource>, DeviceError> {
        self.0.lock().capture_opens += 1;
        Ok(Box::new(PacedSource(self.0.clone())))
    }
}

/// A virtio-snd function behind the real transport, driven like a driver
/// drives it: PCI config, BAR4 common configuration, queue kicks.
struct Guest {
    pci: VirtioPciDevice,
    mem: GuestMemoryMmap<()>,
    device: Arc<Mutex<VirtioSnd>>,
    msi: Arc<RecMsi>,
    irq: Arc<RecIrq>,
    avail_idx: [u16; 4],
    /// Next free descriptor-table entry per queue.
    next_desc: [u16; 4],
}

impl Guest {
    /// A machine with `host`'s audio; `microphone` adds the capture stream.
    fn new(host: Arc<dyn HostAudio>, microphone: bool) -> Self {
        let mem = GuestMemoryMmap::from_ranges(&[(GuestAddress(0), 1 << 20)]).unwrap();
        let msi = Arc::new(RecMsi::default());
        let irq = Arc::new(RecIrq::default());
        let (device, sound) = VirtioSnd::new_shared("virtio-snd@guest", host, microphone);
        let pci = VirtioPciDevice::new(
            "pci@00:05.0",
            Box::new(sound),
            mem.clone(),
            Some(msi.clone()),
            Some(irq.clone()),
        );
        Self {
            pci,
            mem,
            device,
            msi,
            irq,
            avail_idx: [0; 4],
            next_desc: [0; 4],
        }
    }

    fn cfg32(&mut self, off: u64) -> u32 {
        self.pci.config_read((off / 4) as usize)
    }

    fn cfg16_write(&mut self, off: u64, value: u16) {
        self.pci
            .config_write((off / 4) as usize, off % 4, &value.to_le_bytes());
    }

    fn bar_write(&mut self, off: u64, bytes: &[u8]) {
        self.pci.bar_write(4, off, bytes);
    }

    fn bar8_write(&mut self, off: u64, value: u8) {
        self.bar_write(off, &[value]);
    }

    fn bar16_write(&mut self, off: u64, value: u16) {
        self.bar_write(off, &value.to_le_bytes());
    }

    fn bar32_write(&mut self, off: u64, value: u32) {
        self.bar_write(off, &value.to_le_bytes());
    }

    fn bar32_read(&mut self, off: u64) -> u32 {
        let mut bytes = [0u8; 4];
        self.pci.bar_read(4, off, &mut bytes);
        u32::from_le_bytes(bytes)
    }

    fn bar16_read(&mut self, off: u64) -> u16 {
        let mut bytes = [0u8; 2];
        self.pci.bar_read(4, off, &mut bytes);
        u16::from_le_bytes(bytes)
    }

    /// Enable MSI-X with vector 0 recorded by the test sink.
    fn setup_msix(&mut self) {
        self.cfg16_write(0x42, 0x8000);
        self.bar32_write(MSIX_TABLE, 0xFEE0_0000);
        self.bar32_write(MSIX_TABLE + 4, 0);
        self.bar32_write(MSIX_TABLE + 8, 0x41);
    }

    /// Acknowledge, accept VERSION_1 only, and reach FEATURES_OK.
    fn negotiate(&mut self) {
        // The offered set is the device's features plus VERSION_1; the
        // driver takes VERSION_1 and leaves RING_EVENT_IDX out, so every
        // completion raises the queue interrupt.
        self.bar32_write(GUEST_FEATURE_SELECT, 0);
        self.bar32_write(GUEST_FEATURE, 0);
        self.bar32_write(GUEST_FEATURE_SELECT, 1);
        self.bar32_write(GUEST_FEATURE, 1); // the high half of VERSION_1
        self.bar8_write(DEVICE_STATUS, 0x03); // ACKNOWLEDGE | DRIVER
        self.bar8_write(DEVICE_STATUS, 0x0B); // | FEATURES_OK
        assert_ne!(self.bar32_read(DEVICE_STATUS) & 8, 0);
    }

    /// Configure all four queues with their ring addresses and enable.
    fn setup_queues(&mut self) {
        for queue in 0..4u16 {
            let (desc, avail, used) = rings(queue);
            self.bar16_write(QUEUE_SELECT, queue);
            assert_eq!(self.bar16_read(QUEUE_SIZE), 64);
            self.bar16_write(QUEUE_SIZE, SIZE);
            self.bar32_write(QUEUE_DESC, desc as u32);
            self.bar32_write(QUEUE_DESC + 4, 0);
            self.bar32_write(QUEUE_AVAIL, avail as u32);
            self.bar32_write(QUEUE_AVAIL + 4, 0);
            self.bar32_write(QUEUE_USED, used as u32);
            self.bar32_write(QUEUE_USED + 4, 0);
            self.bar16_write(QUEUE_MSIX_VECTOR, 0);
            self.bar16_write(QUEUE_ENABLE, 1);
        }
        self.bar8_write(DEVICE_STATUS, 0x0F); // | DRIVER_OK
    }

    fn notify(&mut self, queue: u16) {
        self.bar_write(NOTIFY + u64::from(queue) * 4, &[0, 0, 0, 0]);
    }

    /// Publish a descriptor chain on `queue` and kick it. Each chain gets
    /// its own descriptor-table slots, as a real driver allocates: a held
    /// message's descriptors must survive later submissions.
    fn submit(&mut self, queue: u16, descriptors: &[(u64, u32, u16)]) {
        let (desc_table, avail, _) = rings(queue);
        let first = self.next_desc[queue as usize];
        for (i, &(addr, len, flags)) in descriptors.iter().enumerate() {
            let mut entry = [0u8; 16];
            entry[0..8].copy_from_slice(&addr.to_le_bytes());
            entry[8..12].copy_from_slice(&len.to_le_bytes());
            entry[12..14].copy_from_slice(
                &(flags | if i + 1 < descriptors.len() { NEXT } else { 0 }).to_le_bytes(),
            );
            entry[14..16].copy_from_slice(
                &(if i + 1 < descriptors.len() {
                    first + i as u16 + 1
                } else {
                    0
                })
                .to_le_bytes(),
            );
            self.mem
                .write_slice(
                    &entry,
                    GuestAddress(desc_table + u64::from(first + i as u16) * 16),
                )
                .unwrap();
        }
        self.next_desc[queue as usize] = first + descriptors.len() as u16;
        let slot = self.avail_idx[queue as usize];
        self.mem
            .write_obj(
                Le16::from(first),
                GuestAddress(avail + 4 + 2 * u64::from(slot)),
            )
            .unwrap();
        self.avail_idx[queue as usize] += 1;
        self.mem
            .write_obj(
                Le16::from(self.avail_idx[queue as usize]),
                GuestAddress(avail + 2),
            )
            .unwrap();
        self.notify(queue);
    }

    /// The used ring index of `queue`.
    fn used_idx(&self, queue: u16) -> u16 {
        let (_, _, used) = rings(queue);
        self.mem
            .read_obj::<Le16>(GuestAddress(used + 2))
            .unwrap()
            .to_native()
    }

    /// One used entry: (descriptor head, written length).
    fn used_entry(&self, queue: u16, slot: u64) -> (u16, u32) {
        let (_, _, used) = rings(queue);
        let id = self
            .mem
            .read_obj::<Le16>(GuestAddress(used + 4 + slot * 8))
            .unwrap()
            .to_native();
        let len = self
            .mem
            .read_obj::<Le32>(GuestAddress(used + 8 + slot * 8))
            .unwrap()
            .to_native();
        (id, len)
    }

    fn write_bytes(&self, addr: u64, bytes: &[u8]) {
        self.mem.write_slice(bytes, GuestAddress(addr)).unwrap();
    }

    fn read_bytes(&self, addr: u64, len: usize) -> Vec<u8> {
        let mut out = vec![0u8; len];
        self.mem.read_slice(&mut out, GuestAddress(addr)).unwrap();
        out
    }

    /// Send one control message and return the full response. Control
    /// processing is synchronous — the device answers before the kick
    /// returns — so the harness reclaims the chain's descriptors, the way
    /// a driver recycles them once the used ring reports them done.
    fn control(&mut self, request: &[u8], response_len: usize) -> Vec<u8> {
        self.write_bytes(CTRL_REQ, request);
        self.mem
            .write_slice(&vec![0u8; response_len], GuestAddress(CTRL_RESP))
            .unwrap();
        let first = self.next_desc[VQ_CONTROL as usize];
        self.submit(
            VQ_CONTROL,
            &[
                (CTRL_REQ, request.len() as u32, NEXT),
                (CTRL_RESP, response_len as u32, WRITE),
            ],
        );
        self.next_desc[VQ_CONTROL as usize] = first;
        self.read_bytes(CTRL_RESP, response_len)
    }

    fn control_status(&mut self, request: &[u8]) -> u32 {
        let response = self.control(request, 4);
        u32::from_le_bytes(response[0..4].try_into().unwrap())
    }

    /// `VIRTIO_SND_R_PCM_SET_PARAMS` for `stream`.
    fn set_params(&mut self, stream: u32, buffer: u32, period: u32) -> u32 {
        let mut request = Vec::new();
        request.extend_from_slice(&R_PCM_SET_PARAMS.to_le_bytes());
        request.extend_from_slice(&stream.to_le_bytes());
        request.extend_from_slice(&buffer.to_le_bytes());
        request.extend_from_slice(&period.to_le_bytes());
        request.extend_from_slice(&0u32.to_le_bytes()); // features: none
        request.push(2); // channels: stereo
        request.push(PCM_FMT_S16);
        request.push(PCM_RATE_48000);
        request.push(0); // padding
        self.control_status(&request)
    }

    fn pcm_command(&mut self, code: u32, stream: u32) -> u32 {
        let mut request = Vec::new();
        request.extend_from_slice(&code.to_le_bytes());
        request.extend_from_slice(&stream.to_le_bytes());
        self.control_status(&request)
    }

    /// Prepare and start both directions the machine has.
    fn start_streams(&mut self) {
        assert_eq!(self.set_params(0, 4096, 1024), S_OK);
        assert_eq!(self.pcm_command(R_PCM_PREPARE, 0), S_OK);
        assert_eq!(self.pcm_command(R_PCM_START, 0), S_OK);
        if self.bar32_read(DEVICE_CFG + 4) >= 2 {
            assert_eq!(self.set_params(1, 4096, 1024), S_OK);
            assert_eq!(self.pcm_command(R_PCM_PREPARE, 1), S_OK);
            assert_eq!(self.pcm_command(R_PCM_START, 1), S_OK);
        }
    }

    /// Post one tx message: xfer header plus PCM, then a status buffer.
    fn tx(&mut self, slot: u64, stream: u32, pcm: &[u8]) {
        let base = TX_BASE + slot * 0x2000;
        let mut message = Vec::with_capacity(4 + pcm.len());
        message.extend_from_slice(&stream.to_le_bytes());
        message.extend_from_slice(pcm);
        self.write_bytes(base, &message);
        self.submit(
            VQ_TX,
            &[
                (base, message.len() as u32, NEXT),
                (base + 0x1000, 8, WRITE),
            ],
        );
    }

    /// Post one rx message: xfer header, a `capacity` capture buffer, then
    /// a status buffer.
    fn rx(&mut self, slot: u64, stream: u32, capacity: u32) {
        let base = RX_BASE + slot * 0x2000;
        self.write_bytes(base, &stream.to_le_bytes());
        self.submit(
            VQ_RX,
            &[
                (base, 4, NEXT),
                (base + 0x10, capacity, NEXT | WRITE),
                (base + 0x10 + u64::from(capacity), 8, WRITE),
            ],
        );
    }

    /// The status written into the tail buffer of tx message `slot`.
    fn tx_status(&self, slot: u64) -> (u32, u32) {
        let base = TX_BASE + slot * 0x2000 + 0x1000;
        let bytes = self.read_bytes(base, 8);
        (
            u32::from_le_bytes(bytes[0..4].try_into().unwrap()),
            u32::from_le_bytes(bytes[4..8].try_into().unwrap()),
        )
    }

    /// The captured PCM and status of rx message `slot`.
    fn rx_payload(&self, slot: u64, capacity: u32) -> (Vec<u8>, (u32, u32)) {
        let base = RX_BASE + slot * 0x2000 + 0x10;
        let status = self.read_bytes(base + u64::from(capacity), 8);
        (
            self.read_bytes(base, capacity as usize),
            (
                u32::from_le_bytes(status[0..4].try_into().unwrap()),
                u32::from_le_bytes(status[4..8].try_into().unwrap()),
            ),
        )
    }

    /// Run the machine clock forward, as the chipset's timer would.
    fn advance(&self, elapsed: Duration) {
        self.device.lock().advance(elapsed).unwrap();
    }
}

/// 256 frames of a recognisable pattern, one per call.
fn period_pcm(marker: u8) -> Vec<u8> {
    vec![marker; 1024]
}

#[test]
fn pci_discovery_and_config_space() {
    let mut g = Guest::new(Arc::new(RecordingHost::new()), true);
    // Virtio-snd is device type 25: PCI 1af4:1059, revision 2.
    assert_eq!(g.cfg32(0x00), (0x1059 << 16) | 0x1AF4);
    assert_eq!(g.cfg32(0x08) & 0xFF, 2);
    g.negotiate();
    assert_eq!(g.bar16_read(NUM_QUEUES), 4);
    // jacks 0, streams 2, chmaps 0, controls 0 (microphone on).
    assert_eq!(g.bar32_read(DEVICE_CFG), 0);
    assert_eq!(g.bar32_read(DEVICE_CFG + 4), 2);
    assert_eq!(g.bar32_read(DEVICE_CFG + 8), 0);
    assert_eq!(g.bar32_read(DEVICE_CFG + 12), 0);

    let mut g = Guest::new(Arc::new(RecordingHost::new()), false);
    g.negotiate();
    assert_eq!(g.bar32_read(DEVICE_CFG + 4), 1);
}

#[test]
fn pcm_info_reports_the_streams() {
    let mut g = Guest::new(Arc::new(RecordingHost::new()), true);
    g.negotiate();
    g.setup_queues();
    let mut request = Vec::new();
    request.extend_from_slice(&R_PCM_INFO.to_le_bytes());
    request.extend_from_slice(&0u32.to_le_bytes()); // start_id
    request.extend_from_slice(&2u32.to_le_bytes()); // count
    request.extend_from_slice(&32u32.to_le_bytes()); // size
    let response = g.control(&request, 4 + 2 * 32);
    assert_eq!(u32::from_le_bytes(response[0..4].try_into().unwrap()), S_OK);
    for (i, expected_direction) in [(0usize, 0u8), (1, 1)] {
        let entry = &response[4 + i * 32..4 + (i + 1) * 32];
        assert_eq!(u32::from_le_bytes(entry[0..4].try_into().unwrap()), 0); // hda_fn_nid
        assert_eq!(u32::from_le_bytes(entry[4..8].try_into().unwrap()), 0); // features
        assert_eq!(
            u64::from_le_bytes(entry[8..16].try_into().unwrap()),
            (1u64 << PCM_FMT_S16) | (1u64 << PCM_FMT_FLOAT)
        );
        assert_eq!(
            u64::from_le_bytes(entry[16..24].try_into().unwrap()),
            (1u64 << PCM_RATE_44100) | (1u64 << PCM_RATE_48000) | (1u64 << PCM_RATE_96000)
        );
        assert_eq!(entry[24], expected_direction);
        assert_eq!(entry[25], 1);
        assert_eq!(entry[26], 2);
        assert!(
            entry[27..32].iter().all(|&b| b == 0),
            "padding must be zero"
        );
    }

    // Out of range: the driver must never ask, the device says BAD_MSG.
    let mut request = Vec::new();
    request.extend_from_slice(&R_PCM_INFO.to_le_bytes());
    request.extend_from_slice(&1u32.to_le_bytes());
    request.extend_from_slice(&2u32.to_le_bytes());
    request.extend_from_slice(&32u32.to_le_bytes());
    assert_eq!(g.control_status(&request), S_BAD_MSG);
}

#[test]
fn pcm_lifecycle_validates_transitions_and_parameters() {
    let mut g = Guest::new(Arc::new(RecordingHost::new()), true);
    g.negotiate();
    g.setup_queues();

    // Nothing works before the parameters are set.
    assert_eq!(g.pcm_command(R_PCM_START, 0), S_BAD_MSG);
    assert_eq!(g.pcm_command(R_PCM_PREPARE, 0), S_BAD_MSG);
    assert_eq!(g.pcm_command(R_PCM_RELEASE, 0), S_BAD_MSG);
    assert_eq!(g.pcm_command(R_PCM_STOP, 0), S_BAD_MSG);

    // Parameter validation: unsupported values are NOT_SUPP, structural
    // nonsense is BAD_MSG, unknown streams are BAD_MSG.
    let mut raw = |buffer: u32, period: u32, features: u32, channels: u8, format: u8, rate: u8| {
        let mut request = Vec::new();
        request.extend_from_slice(&R_PCM_SET_PARAMS.to_le_bytes());
        request.extend_from_slice(&0u32.to_le_bytes());
        request.extend_from_slice(&buffer.to_le_bytes());
        request.extend_from_slice(&period.to_le_bytes());
        request.extend_from_slice(&features.to_le_bytes());
        request.push(channels);
        request.push(format);
        request.push(rate);
        request.push(0);
        g.control_status(&request)
    };
    assert_eq!(
        raw(4096, 1024, 1, 2, PCM_FMT_S16, PCM_RATE_48000),
        S_NOT_SUPP
    );
    assert_eq!(
        raw(4096, 1024, 0, 3, PCM_FMT_S16, PCM_RATE_48000),
        S_NOT_SUPP
    );
    assert_eq!(raw(4096, 1024, 0, 2, 4, PCM_RATE_48000), S_NOT_SUPP);
    assert_eq!(raw(4096, 1024, 0, 2, PCM_FMT_S16, 3), S_NOT_SUPP);
    assert_eq!(raw(1000, 300, 0, 2, PCM_FMT_S16, PCM_RATE_48000), S_BAD_MSG);
    assert_eq!(raw(0, 1024, 0, 2, PCM_FMT_S16, PCM_RATE_48000), S_BAD_MSG);
    assert_eq!(
        raw(4096, 1000, 0, 2, PCM_FMT_S16, PCM_RATE_48000),
        S_BAD_MSG
    );
    assert_eq!(g.set_params(5, 4096, 1024), S_BAD_MSG);

    // The full lifecycle, twice over.
    for _ in 0..2 {
        assert_eq!(g.set_params(0, 4096, 1024), S_OK);
        assert_eq!(g.pcm_command(R_PCM_PREPARE, 0), S_OK);
        assert_eq!(g.pcm_command(R_PCM_START, 0), S_OK);
        // Started: no re-negotiation, no re-prepare, no release.
        assert_eq!(g.set_params(0, 8192, 1024), S_BAD_MSG);
        assert_eq!(g.pcm_command(R_PCM_PREPARE, 0), S_BAD_MSG);
        assert_eq!(g.pcm_command(R_PCM_RELEASE, 0), S_BAD_MSG);
        assert_eq!(g.pcm_command(R_PCM_STOP, 0), S_OK);
        // Stopped: restart or release, nothing else.
        assert_eq!(g.pcm_command(R_PCM_STOP, 0), S_BAD_MSG);
        assert_eq!(g.set_params(0, 8192, 1024), S_OK); // back to Set
        assert_eq!(g.pcm_command(R_PCM_START, 0), S_BAD_MSG);
        // The Linux driver frees parameters it never prepared.
        assert_eq!(g.pcm_command(R_PCM_RELEASE, 0), S_OK);
    }

    // This device has no jacks and no control elements.
    assert_eq!(g.control_status(&R_JACK_INFO.to_le_bytes()), S_NOT_SUPP);
    assert_eq!(g.control_status(&R_CTL_INFO.to_le_bytes()), S_NOT_SUPP);
    assert_eq!(g.control_status(&0x0123_4567u32.to_le_bytes()), S_NOT_SUPP);
}

#[test]
fn tx_completions_follow_host_consumption() {
    let host = PacedHost::default();
    let mut g = Guest::new(Arc::new(host.clone()), true);
    g.setup_msix();
    g.negotiate();
    g.setup_queues();
    g.start_streams();

    g.tx(0, 0, &period_pcm(0xAA));
    g.tx(1, 0, &period_pcm(0xBB));
    g.tx(2, 0, &period_pcm(0xCC));

    // One period fits the watermark: the first message is consumed and
    // completed, with the host's latency in its status.
    g.advance(Duration::from_millis(1));
    assert_eq!(g.used_idx(VQ_TX), 1);
    assert_eq!(g.tx_status(0), (S_OK, 1024 + 123));
    assert_eq!(g.used_entry(VQ_TX, 0).1, 8);
    assert_eq!(host.pcm(), period_pcm(0xAA));

    // The host has not consumed: nothing more completes, however much
    // machine time passes.
    g.advance(Duration::from_millis(50));
    assert_eq!(g.used_idx(VQ_TX), 1);
    assert_eq!(host.pcm(), period_pcm(0xAA));

    // Consumption paces the next completion.
    host.drain(1024);
    g.advance(Duration::from_millis(1));
    assert_eq!(g.used_idx(VQ_TX), 2);
    // The message's own PCM is queued host-side at completion: that plus
    // the graph offset is the latency the guest sees.
    assert_eq!(g.tx_status(1), (S_OK, 1024 + 123));
    assert_eq!(host.pcm(), [period_pcm(0xAA), period_pcm(0xBB)].concat());

    host.drain(2048);
    g.advance(Duration::from_millis(1));
    assert_eq!(g.used_idx(VQ_TX), 3);
    assert_eq!(
        host.pcm(),
        [period_pcm(0xAA), period_pcm(0xBB), period_pcm(0xCC)].concat()
    );

    // The completions raised the tx queue's MSI-X vector.
    assert!(!g.msi.messages.lock().is_empty());
    let _ = g.irq;
}

#[test]
fn tx_pre_buffers_until_started_and_rejects_foreign_streams() {
    let host = PacedHost::default();
    let mut g = Guest::new(Arc::new(host.clone()), true);
    g.negotiate();
    g.setup_queues();
    // Prepared, not started: the lifecycle's pre-buffering window.
    assert_eq!(g.set_params(0, 4096, 1024), S_OK);
    assert_eq!(g.pcm_command(R_PCM_PREPARE, 0), S_OK);

    g.tx(0, 0, &period_pcm(0x11));
    g.tx(1, 7, &period_pcm(0x22)); // no such stream
    g.advance(Duration::from_millis(20));
    assert_eq!(g.used_idx(VQ_TX), 0, "a prepared stream must hold messages");
    assert!(host.pcm().is_empty());

    // Started: the pre-buffered message plays, and the message naming a
    // nonexistent stream completes cancelled.
    assert_eq!(g.pcm_command(R_PCM_START, 0), S_OK);
    g.advance(Duration::from_millis(1));
    assert_eq!(g.used_idx(VQ_TX), 1);
    assert_eq!(g.tx_status(0), (S_OK, 1024 + 123));
    assert_eq!(host.pcm(), period_pcm(0x11));
    host.drain(1024);
    g.advance(Duration::from_millis(1));
    assert_eq!(g.used_idx(VQ_TX), 2);
    assert_eq!(g.tx_status(1), (S_IO_ERR, 0));
    assert_eq!(
        host.pcm(),
        period_pcm(0x11),
        "a cancelled message plays nothing"
    );
}

#[test]
fn capture_paces_from_the_scripted_source() {
    // The recording backend (seam A1): scripted periods, in order.
    let script = vec![period_pcm(0x51), period_pcm(0x52)];
    let host = RecordingHost::with_capture_script(script.clone());
    let mut g = Guest::new(Arc::new(host), true);
    g.negotiate();
    g.setup_queues();
    g.start_streams();

    g.rx(0, 1, 1024);
    g.rx(1, 1, 1024);
    g.rx(2, 1, 1024);

    // A period of machine time fills exactly one message. 256 frames at
    // 48 kHz is 5⅓ ms.
    g.advance(Duration::from_nanos(5_333_334));
    assert_eq!(g.used_idx(VQ_RX), 1);
    let (pcm, status) = g.rx_payload(0, 1024);
    assert_eq!(pcm, period_pcm(0x51));
    assert_eq!(status, (S_OK, 0));
    assert_eq!(g.used_entry(VQ_RX, 0).1, 1024 + 8);

    g.advance(Duration::from_nanos(5_333_334));
    assert_eq!(g.used_idx(VQ_RX), 2);
    assert_eq!(g.rx_payload(1, 1024).0, period_pcm(0x52));

    // The script is exhausted: the host supplies silence, still paced.
    g.advance(Duration::from_nanos(5_333_334));
    assert_eq!(g.used_idx(VQ_RX), 3);
    assert_eq!(g.rx_payload(2, 1024).0, vec![0u8; 1024]);

    // Time does not accumulate without the clock: no more completions.
    g.rx(3, 1, 1024);
    assert_eq!(g.used_idx(VQ_RX), 3);
}

#[test]
fn microphone_off_opens_no_capture_stream() {
    let host = RecordingHost::new();
    let mut g = Guest::new(Arc::new(host.clone()), false);
    g.negotiate();
    g.setup_queues();
    g.start_streams(); // playback only: stream 1 does not exist

    g.rx(0, 1, 1024);
    g.advance(Duration::from_millis(100));
    // The message completes cancelled, and no host capture stream opened.
    assert_eq!(g.used_idx(VQ_RX), 1);
    assert_eq!(g.rx_payload(0, 1024).1, (S_IO_ERR, 0));
    assert!(!host.capture_opened(), "the microphone is off (D1)");
}

#[test]
fn stop_freezes_and_release_completes_the_pending() {
    let host = PacedHost::default();
    let mut g = Guest::new(Arc::new(host.clone()), true);
    g.negotiate();
    g.setup_queues();
    g.start_streams();

    g.tx(0, 0, &period_pcm(0x0A));
    g.tx(1, 0, &period_pcm(0x0B));
    g.advance(Duration::from_millis(1));
    assert_eq!(g.used_idx(VQ_TX), 1);

    // STOP: the second message is held, its PCM never reaches the host.
    assert_eq!(g.pcm_command(R_PCM_STOP, 0), S_OK);
    host.drain(1024);
    g.advance(Duration::from_millis(1));
    g.advance(Duration::from_millis(1));
    assert_eq!(g.used_idx(VQ_TX), 1, "a stopped stream holds its messages");

    // RELEASE completes every pending message before answering: the used
    // entry lands before the control response, cancelled.
    assert_eq!(g.pcm_command(R_PCM_RELEASE, 0), S_OK);
    assert_eq!(g.used_idx(VQ_TX), 2, "RELEASE must complete pending I/O");
    assert_eq!(g.tx_status(1), (S_IO_ERR, 0));
    assert_eq!(host.pcm(), period_pcm(0x0A));
}

#[test]
fn pause_freezes_both_directions() {
    let host = PacedHost::with_script(vec![period_pcm(0x77)]);
    let mut g = Guest::new(Arc::new(host.clone()), true);
    g.negotiate();
    g.setup_queues();
    g.start_streams();

    // The tx kick drains opportunistically: with the host queue empty one
    // period completes at once, before the machine pauses.
    g.tx(0, 0, &period_pcm(0x33));
    assert_eq!(g.used_idx(VQ_TX), 1);
    assert_eq!(host.pcm(), period_pcm(0x33));
    g.rx(0, 1, 1024);
    g.device.lock().set_paused(true);
    g.advance(Duration::from_secs(1));
    assert_eq!(g.used_idx(VQ_TX), 1, "a paused machine completes nothing");
    assert_eq!(g.used_idx(VQ_RX), 0);
    assert!(host.pulls().is_empty(), "a paused machine captures nothing");

    // Resumed: a paused machine loses no time and drops no message.
    g.device.lock().set_paused(false);
    g.advance(Duration::from_nanos(5_333_334));
    assert_eq!(g.used_idx(VQ_TX), 1);
    assert_eq!(g.used_idx(VQ_RX), 1);
    assert_eq!(g.rx_payload(0, 1024).0, period_pcm(0x77));
}

#[test]
fn snapshot_round_trip_reopens_streams_and_discards_capture() {
    let host = PacedHost::with_script(vec![period_pcm(0x91), period_pcm(0x92), period_pcm(0x93)]);
    let mut g = Guest::new(Arc::new(host.clone()), true);
    g.negotiate();
    g.setup_queues();
    g.start_streams();

    // Mid-flight: one tx period played and one waiting; one rx period
    // delivered, two more banked in the capture accumulator.
    g.tx(0, 0, &period_pcm(0xD1));
    g.tx(1, 0, &period_pcm(0xD2));
    g.rx(0, 1, 1024);
    g.advance(Duration::from_nanos(5_333_334));
    assert_eq!(g.used_idx(VQ_TX), 1);
    assert_eq!(g.used_idx(VQ_RX), 1);
    assert_eq!(g.rx_payload(0, 1024).0, period_pcm(0x91));

    let state = g.device.lock().save_state().unwrap();

    // Restore, as the VMM does: the same objects, host state lost.
    g.device.lock().restore_state(&state).unwrap();
    assert_eq!(host.playback_opens(), 2, "restore reopens playback");
    assert_eq!(host.capture_opens(), 2, "restore reopens capture");
    // One period of capture was discarded: the script advanced past 0x92.
    assert_eq!(host.pulls(), vec![1024, 1024]);

    // The held tx message is still in the restored queue and plays once
    // the (fresh) host queue drains.
    host.drain(1024);
    g.advance(Duration::from_millis(1));
    assert_eq!(g.used_idx(VQ_TX), 2);
    assert_eq!(g.tx_status(1), (S_OK, 1024 + 123));
    assert_eq!(host.pcm(), [period_pcm(0xD1), period_pcm(0xD2)].concat());

    // Capture resumes from the script's next period: 0x93.
    g.rx(1, 1, 1024);
    g.advance(Duration::from_nanos(5_333_334));
    assert_eq!(g.rx_payload(1, 1024).0, period_pcm(0x93));
}

#[test]
fn driver_reset_returns_the_device_to_power_on() {
    let host = PacedHost::default();
    let mut g = Guest::new(Arc::new(host.clone()), true);
    g.negotiate();
    g.setup_queues();
    g.start_streams();

    // Status 0 resets the device through the transport.
    g.bar8_write(DEVICE_STATUS, 0);
    assert_eq!(g.bar32_read(DEVICE_CFG + 4), 2, "the streams are static");
    // A fresh negotiation works, and the control queue answers from a
    // clean lifecycle.
    g.negotiate();
    g.setup_queues();
    assert_eq!(g.pcm_command(R_PCM_START, 0), S_BAD_MSG);
    assert_eq!(g.set_params(0, 4096, 1024), S_OK);
    assert_eq!(g.pcm_command(R_PCM_PREPARE, 0), S_OK);
    assert_eq!(g.pcm_command(R_PCM_START, 0), S_OK);
    assert_eq!(
        host.playback_opens(),
        2,
        "the reset closed the first stream"
    );
}

// The transport must see the wrapped device type even through the shared
// handle.
#[test]
fn shared_handle_is_a_virtio_device_of_type_25() {
    let (_, handle): (_, SoundPci) =
        VirtioSnd::new_shared("virtio-snd@x", Arc::new(RecordingHost::new()), false);
    assert_eq!(VirtioDevice::device_type(&handle), 25);
    assert_eq!(handle.queue_max_sizes(), &[64, 64, 64, 64]);
    assert_eq!(handle.config_len(), 16);
    assert_eq!(handle.state_id(), "virtio-snd@x");
}
