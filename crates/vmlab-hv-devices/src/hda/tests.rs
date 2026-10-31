use super::*;
use crate::audio::recording::RecordingHost;
use parking_lot::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

#[derive(Default)]
struct Interrupts(Mutex<Vec<MsiMessage>>);
impl MsiSink for Interrupts {
    fn signal(&self, message: MsiMessage) {
        self.0.lock().push(message);
    }
}

fn fixture(host: Arc<dyn HostAudio>, microphone: bool) -> (Hda, GuestMemory, Arc<Interrupts>) {
    let memory = GuestMemory::from_ranges(&[(GuestAddress(0), 0x20000)]).unwrap();
    let interrupts = Arc::new(Interrupts::default());
    let mut hda = Hda::new("hda", memory.clone(), host, interrupts.clone(), microphone);
    hda.config_write(1, 0, &[6, 0]);
    write(&mut hda, 8, 1, 4);
    (hda, memory, interrupts)
}

fn write(hda: &mut Hda, offset: u64, value: u32, width: usize) {
    hda.bar_write(0, offset, &value.to_le_bytes()[..width]);
}
fn read(hda: &mut Hda, offset: u64, width: usize) -> u32 {
    let mut value = [0; 4];
    hda.bar_read(0, offset, &mut value[..width]);
    u32::from_le_bytes(value)
}
fn verb(hda: &mut Hda, node: u8, command: u32) -> u32 {
    write(hda, 0x60, (u32::from(node) << 20) | command, 4);
    write(hda, 0x68, 3, 2);
    assert_eq!(read(hda, 0x68, 2), 2);
    read(hda, 0x64, 4)
}
fn setup_stream(hda: &mut Hda, mem: &GuestMemory, input: bool, length: u32) {
    let (base, bdl, buffer, node) = if input {
        (0x80, 0x3000, 0x8000, 4)
    } else {
        (0xa0, 0x2000, 0x4000, 2)
    };
    let mut descriptor = [0; 16];
    descriptor[..8].copy_from_slice(&(buffer as u64).to_le_bytes());
    descriptor[8..12].copy_from_slice(&length.to_le_bytes());
    descriptor[12..].copy_from_slice(&1u32.to_le_bytes());
    mem.write_slice(&descriptor, GuestAddress(bdl)).unwrap();
    verb(hda, node, 0x70610);
    verb(hda, node, 0x20011);
    verb(hda, node, if input { 0x3704a } else { 0x3b04a });
    write(hda, base + 8, length, 4);
    write(hda, base + 0xc, 0, 2);
    write(hda, base + 0x12, 0x11, 2);
    write(hda, base + 0x18, bdl as u32, 4);
    write(hda, base + 2, 0x10, 1);
    write(hda, base, 0x1e, 1);
}
fn interval(hda: &mut Hda) {
    hda.advance(Duration::from_millis(1)).unwrap();
}
fn samples(left: i16, right: i16, frames: usize) -> Vec<u8> {
    [left.to_le_bytes(), right.to_le_bytes()]
        .concat()
        .repeat(frames)
}

#[test]
fn pci_discovery_reset_and_register_permissions() {
    let (mut hda, _, _) = fixture(Arc::new(RecordingHost::new()), false);
    assert_eq!(hda.config_read(0), 0x293e8086);
    assert_eq!(hda.config_read(2) >> 8, 0x040300);
    assert_eq!(
        hda.bars(),
        vec![BarInfo {
            index: 0,
            size: 16384,
            kind: BarKind::Mmio32
        }]
    );
    hda.config_write(4, 0, &u32::MAX.to_le_bytes());
    assert_eq!(hda.config_read(4), 0xffffc000);
    hda.config_write(0, 0, &[0; 4]);
    assert_eq!(hda.config_read(0), 0x293e8086);
    assert_eq!(read(&mut hda, 0, 2), 0x1101);
    write(&mut hda, 0, u32::MAX, 4);
    assert_eq!(read(&mut hda, 0, 2), 0x1101);
    assert_eq!(read(&mut hda, 0xe, 2), 1);
    write(&mut hda, 0xe, 1, 2);
    assert_eq!(read(&mut hda, 0xe, 2), 0);
    write(&mut hda, 8, 0, 4);
    write(&mut hda, 8, 1, 4);
    assert_eq!(read(&mut hda, 0xe, 2), 1);
    write(&mut hda, 0xa0, 1, 1);
    assert_eq!(read(&mut hda, 0xa0, 1), 1);
    assert_eq!(read(&mut hda, 0xa8, 4), 0);
    write(&mut hda, 0xa0, 0, 1);
    assert_eq!(read(&mut hda, 0xa0, 1), 0);
    assert_eq!(read(&mut hda, u64::MAX, 4), 0);
    write(&mut hda, u64::MAX, 0, 4);
}

#[test]
fn codec_discovery_and_stateful_verbs_via_immediate_commands() {
    let (mut hda, _, _) = fixture(Arc::new(RecordingHost::new()), true);
    for (node, parameter, expected) in [
        (0, 0, 0x1af40022),
        (0, 2, 0x100101),
        (0, 4, 0x10001),
        (1, 4, 0x20004),
        (1, 5, 1),
        (2, 9, 0x41d),
        (3, 9, 0x400501),
        (4, 9, 0x10051b),
        (5, 9, 0x400401),
        (2, 0xa, 0x20160),
        (2, 0xb, 1),
        (4, 0xd, 0x80034a4a),
        (3, 0xe, 1),
    ] {
        assert_eq!(verb(&mut hda, node, 0xf0000 | parameter), expected);
    }
    assert_eq!(verb(&mut hda, 3, 0xf0200), 2);
    assert_eq!(verb(&mut hda, 4, 0xf0200), 5);
    assert_eq!(verb(&mut hda, 3, 0xf1c00), 0x4010);
    assert_eq!(verb(&mut hda, 5, 0xf1c00), 0x805020);
    for (node, set, get, expected) in [
        (3, 0x70700, 0xf0700, 0),
        (5, 0x708a9, 0xf0800, 0xa9),
        (1, 0x70503, 0xf0500, 0x33),
        (3, 0x70c00, 0xf0c00, 0),
        (2, 0x70621, 0xf0600, 0x21),
        (4, 0x24011, 0xa0000, 0x4011),
    ] {
        verb(&mut hda, node, set);
        assert_eq!(verb(&mut hda, node, get), expected);
    }
    verb(&mut hda, 1, 0x7ff00);
    assert_eq!(verb(&mut hda, 1, 0xf0500), 0);
    assert_eq!(verb(&mut hda, 3, 0xf0700), 0x40);
    write(&mut hda, 0x68, 2, 2);
    assert_eq!(read(&mut hda, 0x68, 2), 0);
}

/// The PCM parameter must decode, the way the kernel's
/// `snd_hdac_query_supported_pcm` does, to exactly the codec's contract:
/// 16-bit samples at 44.1, 48 and 96 kHz. The width bits are 16-20 of
/// AC_PAR_PCM (bit 16 is *8-bit*, which maps to `SNDRV_PCM_FORMAT_U8`);
/// getting them wrong leaves a real guest with no 16-bit format at all —
/// found against snd_hda_intel on the ubuntu-26.04 template.
#[test]
fn pcm_parameter_decodes_to_16_bit_44_1_48_and_96_khz() {
    let (mut hda, _, _) = fixture(Arc::new(RecordingHost::new()), true);
    let value = verb(&mut hda, 2, 0xf000a);
    let rates = [
        (0u32, 8000),
        (1, 11025),
        (2, 16000),
        (3, 22050),
        (4, 32000),
        (5, 44100),
        (6, 48000),
        (7, 88200),
        (8, 96000),
        (9, 176400),
        (10, 192000),
    ];
    let supported: Vec<u32> = rates
        .iter()
        .filter(|(bit, _)| value & (1 << bit) != 0)
        .map(|(_, hz)| *hz)
        .collect();
    assert_eq!(supported, [44100, 48000, 96000], "rate bits of {value:#x}");
    assert_eq!(value >> 16, 2, "exactly 16-bit width, no other");
}

#[test]
fn corb_rirb_dma_wrap_pointer_reset_and_msi_acknowledgement() {
    let (mut hda, mem, interrupts) = fixture(Arc::new(RecordingHost::new()), true);
    hda.config_write(0x64 / 4, 0, &0xfee00000u32.to_le_bytes());
    hda.config_write(0x6c / 4, 0, &0x45u16.to_le_bytes());
    hda.config_write(0x60 / 4, 2, &[0x81]);
    write(&mut hda, 0x20, 0xc0000000, 4);
    write(&mut hda, 0x40, 0x1000, 4);
    write(&mut hda, 0x50, 0x1800, 4);
    write(&mut hda, 0x4e, 0, 1);
    write(&mut hda, 0x5e, 0, 1);
    write(&mut hda, 0x4a, 0x8000, 2);
    assert_eq!(read(&mut hda, 0x4a, 2), 0x8000);
    write(&mut hda, 0x4a, 0, 2);
    write(&mut hda, 0x58, 0x8000, 2);
    assert_eq!(read(&mut hda, 0x58, 2), 0);
    write(&mut hda, 0x5a, 1, 2);
    write(&mut hda, 0x5c, 7, 1);
    write(&mut hda, 0x4c, 3, 1);
    for (pointer, command, expected) in [(1, 0xf0000u32, 0x1af40022u32), (0, 0x1f0004, 0x20004)] {
        mem.write_slice(&command.to_le_bytes(), GuestAddress(0x1000 + pointer * 4))
            .unwrap();
        write(&mut hda, 0x48, pointer as u32, 2);
        assert_eq!(read(&mut hda, 0x4a, 2), pointer as u32);
        assert_eq!(read(&mut hda, 0x58, 2), pointer as u32);
        let mut response = [0; 8];
        mem.read_slice(&mut response, GuestAddress(0x1800 + pointer * 8))
            .unwrap();
        assert_eq!(
            u32::from_le_bytes(response[..4].try_into().unwrap()),
            expected
        );
        assert_eq!(&response[4..], &[0; 4]);
        assert_eq!(read(&mut hda, 0x24, 4), 0xc0000000);
        write(&mut hda, 0x5d, 1, 1);
        assert_eq!(read(&mut hda, 0x24, 4), 0);
    }
    assert_eq!(
        *interrupts.0.lock(),
        vec![
            MsiMessage {
                address: 0xfee00000,
                data: 0x45
            };
            2
        ]
    );
}

/// Linux opens playback with `bdl_pos_adj`: the first BDL entry is a
/// one-frame nibble that raises an early completion, so the *second*
/// entry's buffer address is not 128-byte aligned. The controller must
/// walk it anyway (found against snd_hda_intel on the ubuntu-26.04
/// template, where the old 128-byte check errored the stream on the first
/// period).
#[test]
fn bdl_with_a_position_adjustment_entry_plays_without_error() {
    let host = RecordingHost::new();
    let (mut hda, mem, _) = fixture(Arc::new(host.clone()), false);
    // Entry 0: one frame at the buffer start, interrupt on completion.
    // Entry 1: the first period's remainder, starting four bytes in.
    // Entry 2: the second period, itself unaligned by the leading frame.
    let mut entry = |index: usize, buffer: u64, length: u32| {
        let mut descriptor = [0; 16];
        descriptor[..8].copy_from_slice(&buffer.to_le_bytes());
        descriptor[8..12].copy_from_slice(&length.to_le_bytes());
        descriptor[12..].copy_from_slice(&1u32.to_le_bytes());
        mem.write_slice(&descriptor, GuestAddress(0x2000 + index as u64 * 16))
            .unwrap();
    };
    entry(0, 0x4000, 4);
    entry(1, 0x4004, 0x3fc);
    entry(2, 0x4404, 0x400);
    let playback = samples(-500, 500, 512);
    mem.write_slice(&playback, GuestAddress(0x4000)).unwrap();
    // Same converter and pin bring-up as setup_stream, with a custom BDL.
    verb(&mut hda, 2, 0x70610);
    verb(&mut hda, 2, 0x20011);
    verb(&mut hda, 2, 0x3b04a);
    write(&mut hda, 0xa8, 0xc00, 4);
    write(&mut hda, 0xac, 2, 2);
    write(&mut hda, 0xb2, 0x11, 2);
    write(&mut hda, 0xb8, 0x2000, 4);
    write(&mut hda, 0xa2, 0x10, 1);
    write(&mut hda, 0xa0, 0x1e, 1);
    // One interval serves 48 frames; 64 of them wrap the 768-frame cyclic
    // buffer eight times. What must hold: no stream error, the first
    // period's PCM intact, and the full runtime's worth of bytes moved.
    for _ in 0..64 {
        interval(&mut hda);
    }
    assert_eq!(read(&mut hda, 0xa3, 1) & 0x10, 0, "no stream error");
    let dumped = host.playback_pcm();
    assert!(
        dumped.starts_with(&playback[..192]),
        "the first period flows through the unaligned descriptors"
    );
    assert_eq!(dumped.len(), 64 * 192);
}

#[test]
fn bdl_duplex_pcm_periods_positions_pause_and_snapshot() {
    let capture = samples(901, -902, 48);
    let host = RecordingHost::with_capture_script(vec![
        capture.clone(),
        vec![0x77; 1024],
        capture.clone(),
    ]);
    let (mut hda, mem, _) = fixture(Arc::new(host.clone()), true);
    let playback = samples(-1234, 2345, 4096);
    mem.write_slice(&playback, GuestAddress(0x4000)).unwrap();
    setup_stream(&mut hda, &mem, false, playback.len() as u32);
    setup_stream(&mut hda, &mem, true, 8192);
    write(&mut hda, 0x70, 0x1001, 4);
    interval(&mut hda);
    assert_eq!(host.playback_pcm(), playback[..192]);
    let periods = host.periods();
    assert_eq!(periods.len(), 1);
    let mut captured = [0; 192];
    mem.read_slice(&mut captured, GuestAddress(0x8000)).unwrap();
    assert_eq!(&captured, capture.as_slice());
    assert_eq!(read(&mut hda, 0xa4, 4), 192);
    assert_eq!(read(&mut hda, 0x84, 4), 192);
    let mut position = [0; 8];
    mem.read_slice(&mut position, GuestAddress(0x1008)).unwrap();
    assert_eq!(u64::from_le_bytes(position), 192);
    let wall = read(&mut hda, 0x30, 4);
    hda.set_paused(true);
    hda.advance(Duration::from_secs(10)).unwrap();
    assert_eq!(read(&mut hda, 0xa4, 4), 192);
    assert_eq!(read(&mut hda, 0x30, 4), wall);
    let state = hda.save_state().unwrap();
    let mut restored = Hda::new(
        "hda",
        mem.clone(),
        Arc::new(host.clone()),
        Arc::new(Interrupts::default()),
        true,
    );
    restored.restore_state(&state).unwrap();
    assert_eq!(restored.save_state().unwrap(), state);
    interval(&mut restored);
    assert_eq!(host.periods().len(), 1);
    restored.set_paused(false);
    interval(&mut restored);
    assert_eq!(host.playback_pcm(), playback[..384]);
    assert!(host.periods()[1].at >= periods[0].at);
    mem.read_slice(&mut captured, GuestAddress(0x8000 + 192))
        .unwrap();
    assert_eq!(&captured, capture.as_slice());
    assert_eq!(read(&mut restored, 0xa4, 4), 384);
    assert_eq!(read(&mut restored, 0x30, 4), wall + 24000);
}

#[test]
fn microphone_off_never_opens_capture_even_after_restore() {
    let host = RecordingHost::with_capture_script(vec![vec![0xff; 1024]]);
    let (mut hda, mem, _) = fixture(Arc::new(host.clone()), false);
    setup_stream(&mut hda, &mem, true, 4096);
    interval(&mut hda);
    let mut pcm = [1; 192];
    mem.read_slice(&mut pcm, GuestAddress(0x8000)).unwrap();
    assert_eq!(pcm, [0; 192]);
    let state = hda.save_state().unwrap();
    hda.restore_state(&state).unwrap();
    interval(&mut hda);
    assert!(!host.capture_opened());
}

#[test]
fn per_channel_amplifier_gain_and_mute_are_applied_to_pcm() {
    let host = RecordingHost::new();
    let (mut hda, mem, _) = fixture(Arc::new(host.clone()), false);
    mem.write_slice(&samples(10000, -10000, 2048), GuestAddress(0x4000))
        .unwrap();
    setup_stream(&mut hda, &mem, false, 8192);
    verb(&mut hda, 2, 0x3a044); // output left, -6 dB
    verb(&mut hda, 2, 0x390ca); // output right, muted
    assert_eq!(verb(&mut hda, 2, 0xba000), 0x44);
    assert_eq!(verb(&mut hda, 2, 0xb8000), 0xca);
    interval(&mut hda);
    assert_eq!(host.periods()[0].pcm, samples(5012, 0, 48));
    verb(&mut hda, 2, 0x3b0ca);
    interval(&mut hda);
    assert_eq!(host.periods()[1].pcm, vec![0; 192]);
}

#[test]
fn stream_tag_format_pin_and_power_gate_playback() {
    let host = RecordingHost::new();
    let (mut hda, mem, _) = fixture(Arc::new(host.clone()), false);
    let pcm = samples(1234, -5678, 4096);
    mem.write_slice(&pcm, GuestAddress(0x4000)).unwrap();
    setup_stream(&mut hda, &mem, false, pcm.len() as u32);
    interval(&mut hda);
    assert_eq!(host.periods()[0].pcm, pcm[..192]);
    for (node, disable, enable) in [
        (2, 0x70620, 0x70610),
        (2, 0x24011, 0x20011),
        (3, 0x70700, 0x70740),
        (2, 0x70503, 0x70500),
        (1, 0x70503, 0x70500),
        (3, 0x70503, 0x70500),
        (3, 0x70c00, 0x70c02),
    ] {
        verb(&mut hda, node, disable);
        interval(&mut hda);
        assert_eq!(host.periods().last().unwrap().pcm, vec![0; 192]);
        verb(&mut hda, node, enable);
        interval(&mut hda);
        assert_eq!(host.periods().last().unwrap().pcm, pcm[..192]);
    }
}

struct BufferedHost {
    latency: Arc<AtomicUsize>,
    periods: Arc<Mutex<Vec<Vec<u8>>>>,
}
struct BufferedSink {
    latency: Arc<AtomicUsize>,
    periods: Arc<Mutex<Vec<Vec<u8>>>>,
}
impl PlaybackSink for BufferedSink {
    fn write_period(&mut self, pcm: &[u8]) {
        self.periods.lock().push(pcm.to_vec());
        self.latency.fetch_add(pcm.len(), Ordering::SeqCst);
    }
    fn latency_bytes(&self) -> usize {
        self.latency.load(Ordering::SeqCst)
    }
}
impl HostAudio for BufferedHost {
    fn name(&self) -> &'static str {
        "buffered-test"
    }
    fn open_playback(&self, _: AudioFormat) -> Result<Box<dyn PlaybackSink>, DeviceError> {
        Ok(Box::new(BufferedSink {
            latency: self.latency.clone(),
            periods: self.periods.clone(),
        }))
    }
    fn open_capture(&self, _: AudioFormat) -> Result<Box<dyn CaptureSource>, DeviceError> {
        Err(DeviceError::Other("capture is not enabled".into()))
    }
}

#[test]
fn position_and_completion_interrupt_follow_host_consumption_not_submission() {
    let latency = Arc::new(AtomicUsize::new(0));
    let periods = Arc::new(Mutex::new(vec![]));
    let host = BufferedHost {
        latency: latency.clone(),
        periods: periods.clone(),
    };
    let (mut hda, mem, interrupts) = fixture(Arc::new(host), false);
    setup_stream(&mut hda, &mem, false, 1024);
    hda.config_write(0x60 / 4, 2, &[0x81]);
    write(&mut hda, 0x20, 0x80000002, 4);
    hda.advance(Duration::from_millis(10)).unwrap();
    assert_eq!(latency.load(Ordering::SeqCst), 1024);
    assert_eq!(read(&mut hda, 0xa4, 4), 0);
    assert_eq!(read(&mut hda, 0xa3, 1) & 4, 0);
    assert!(interrupts.0.lock().is_empty());
    hda.advance(Duration::from_millis(10)).unwrap();
    assert_eq!(periods.lock().len(), 1);
    latency.store(512, Ordering::SeqCst);
    hda.advance(Duration::ZERO).unwrap();
    assert_eq!(read(&mut hda, 0xa4, 4), 512);
    latency.store(900, Ordering::SeqCst);
    hda.advance(Duration::ZERO).unwrap();
    assert_eq!(read(&mut hda, 0xa4, 4), 512);
    latency.store(0, Ordering::SeqCst);
    hda.advance(Duration::ZERO).unwrap();
    assert_eq!(read(&mut hda, 0xa4, 4), 0); // cyclic wrap
    assert_eq!(read(&mut hda, 0xa3, 1) & 4, 4);
    assert_eq!(interrupts.0.lock().len(), 1);
    write(&mut hda, 0xa3, 4, 1);
    assert_eq!(read(&mut hda, 0x24, 4), 0);
}

#[test]
fn malformed_dma_and_unsupported_formats_report_errors_without_panicking() {
    let host = RecordingHost::new();
    let (mut hda, mem, _) = fixture(Arc::new(host.clone()), false);
    setup_stream(&mut hda, &mem, false, 4096);
    mem.write_slice(&[0; 16], GuestAddress(0x2000)).unwrap();
    interval(&mut hda);
    assert_eq!(read(&mut hda, 0xa3, 1) & 16, 16);
    assert_eq!(read(&mut hda, 0xa0, 1) & 2, 0);
    assert!(host.periods().is_empty());
    write(&mut hda, 0xa3, 16, 1);
    write(&mut hda, 0xb2, 0x31, 2); // 32-bit unsupported
    write(&mut hda, 0xa0, 0x1e, 1);
    assert_eq!(read(&mut hda, 0xa3, 1) & 16, 16);
    write(&mut hda, 0x40, 0xfffff000, 4);
    write(&mut hda, 0x4c, 3, 1);
    write(&mut hda, 0x48, 1, 2);
    assert_eq!(read(&mut hda, 0x4d, 1), 1);
}

#[test]
fn snapshots_reject_truncation_version_configuration_and_format_before_mutation() {
    let (mut hda, mem, _) = fixture(Arc::new(RecordingHost::new()), false);
    setup_stream(&mut hda, &mem, false, 4096);
    interval(&mut hda);
    let state = hda.save_state().unwrap();
    let mut bad_states = vec![state[..10].to_vec(), [state.clone(), vec![0]].concat()];
    for (offset, byte) in [
        (0, 0),
        (4, 1),
        (6 + 256 + 0xb2, 0x31),
        (6 + 512 + 2 * 24 + 10, 0x31),
    ] {
        let mut bad = state.clone();
        bad[offset] = byte;
        bad_states.push(bad);
    }
    for bad in bad_states {
        assert!(hda.restore_state(&bad).is_err());
        assert_eq!(hda.save_state().unwrap(), state);
    }
}

#[test]
fn multiple_bdl_entries_split_periods_wrap_and_complete_ioc() {
    let host = RecordingHost::new();
    let (mut hda, mem, _) = fixture(Arc::new(host.clone()), false);
    setup_stream(&mut hda, &mem, false, 384);
    write(&mut hda, 0xa0, 0, 1);
    let first = samples(123, -321, 16);
    let second = samples(456, -654, 80);
    mem.write_slice(&first, GuestAddress(0x4000)).unwrap();
    mem.write_slice(&second, GuestAddress(0x5000)).unwrap();
    for (index, address, length) in [(0, 0x4000u64, 64u32), (1, 0x5000, 320)] {
        let mut descriptor = [0; 16];
        descriptor[..8].copy_from_slice(&address.to_le_bytes());
        descriptor[8..12].copy_from_slice(&length.to_le_bytes());
        descriptor[12..].copy_from_slice(&1u32.to_le_bytes());
        mem.write_slice(&descriptor, GuestAddress(0x2000 + index * 16))
            .unwrap();
    }
    write(&mut hda, 0xac, 1, 2);
    write(&mut hda, 0xa0, 0x1e, 1);
    interval(&mut hda);
    assert_eq!(
        host.periods()[0].pcm,
        [first.clone(), second[..128].to_vec()].concat()
    );
    assert_eq!(read(&mut hda, 0xa4, 4), 192);
    assert_eq!(read(&mut hda, 0xa3, 1) & 4, 4);
    write(&mut hda, 0xa3, 4, 1);
    interval(&mut hda);
    assert_eq!(host.periods()[1].pcm, second[128..]);
    assert_eq!(read(&mut hda, 0xa4, 4), 0);
    assert_eq!(read(&mut hda, 0xa3, 1) & 4, 4);
    interval(&mut hda);
    assert_eq!(
        host.playback_pcm(),
        [first.clone(), second.clone(), first, second[..128].to_vec()].concat()
    );
}

struct GraphHost(Arc<AtomicUsize>);
struct GraphSink(Arc<AtomicUsize>);
impl HostAudio for GraphHost {
    fn name(&self) -> &'static str {
        "graph-test"
    }
    fn open_playback(&self, _: AudioFormat) -> Result<Box<dyn PlaybackSink>, DeviceError> {
        Ok(Box::new(GraphSink(self.0.clone())))
    }
    fn open_capture(&self, _: AudioFormat) -> Result<Box<dyn CaptureSource>, DeviceError> {
        Err(DeviceError::Other("no capture".into()))
    }
}
impl PlaybackSink for GraphSink {
    fn write_period(&mut self, pcm: &[u8]) {
        self.0.fetch_add(pcm.len(), Ordering::SeqCst);
    }
    fn queued_bytes(&self) -> usize {
        self.0.load(Ordering::SeqCst)
    }
    fn latency_bytes(&self) -> usize {
        self.queued_bytes() + 2048
    }
}

#[test]
fn fixed_downstream_graph_latency_does_not_stall_submission() {
    let queued = Arc::new(AtomicUsize::new(0));
    let (mut hda, mem, _) = fixture(Arc::new(GraphHost(queued.clone())), false);
    setup_stream(&mut hda, &mem, false, 8192);
    for _ in 0..12 {
        interval(&mut hda);
        assert_eq!(queued.swap(0, Ordering::SeqCst), 192);
    }
    hda.advance(Duration::ZERO).unwrap();
    assert_eq!(read(&mut hda, 0xa4, 4), 12 * 192 - 2048);
    let snapshot = hda.save_state().unwrap();
    hda.restore_state(&snapshot).unwrap();
}

#[test]
fn wall_clock_does_not_go_backwards_when_restoring_an_older_snapshot() {
    let (mut hda, _, _) = fixture(Arc::new(RecordingHost::new()), false);
    interval(&mut hda);
    let snapshot = hda.save_state().unwrap();
    interval(&mut hda);
    let wall = read(&mut hda, 0x30, 4);
    hda.restore_state(&snapshot).unwrap();
    assert_eq!(read(&mut hda, 0x30, 4), wall);
    interval(&mut hda);
    assert_eq!(read(&mut hda, 0x30, 4), wall + 24000);
}
