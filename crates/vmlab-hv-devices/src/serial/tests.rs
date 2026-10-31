use std::io::Read;
use std::os::unix::net::UnixStream;
use std::time::Duration;

use super::*;

const CHARACTER_NS: u64 = 86_806;

#[derive(Clone, Default)]
struct Recording {
    level: Arc<Mutex<bool>>,
    bytes: Arc<Mutex<Vec<u8>>>,
    blocked: Arc<Mutex<bool>>,
}

struct RecWriter(Recording);

impl Write for RecWriter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if *self.0.blocked.lock() {
            return Err(io::ErrorKind::WouldBlock.into());
        }
        self.0.bytes.lock().extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

struct RecIrq(Recording);

impl IrqLine for RecIrq {
    fn set_level(&self, high: bool) {
        *self.0.level.lock() = high;
    }
}

struct CountingIrq(Arc<Mutex<Vec<bool>>>);

impl IrqLine for CountingIrq {
    fn set_level(&self, high: bool) {
        self.0.lock().push(high);
    }
}

fn counting_serial() -> (Serial, Arc<Mutex<Vec<bool>>>) {
    let levels = Arc::new(Mutex::new(Vec::new()));
    let serial = Serial::new(
        "serial-counted",
        SerialOut::new(Box::new(io::sink())),
        Arc::new(CountingIrq(levels.clone())),
    );
    (serial, levels)
}

#[test]
fn unchanged_running_time_and_register_accesses_do_not_redrive_irq() {
    let (mut serial, levels) = counting_serial();
    assert_eq!(*levels.lock(), [false]);
    for _ in 0..128 {
        serial.advance(1_000);
        read(&mut serial, 5);
    }
    assert_eq!(*levels.lock(), [false]);
    write(&mut serial, 1, 1);
    serial.input_byte(b'x');
    for _ in 0..128 {
        serial.advance(1_000);
        read(&mut serial, 5);
    }
    assert_eq!(*levels.lock(), [false, true]);
    assert_eq!(read(&mut serial, 0), b'x');
    assert_eq!(*levels.lock(), [false, true, false]);
}

#[test]
fn reset_and_restore_force_irq_output_even_when_the_cached_level_matches() {
    let (mut serial, levels) = counting_serial();
    let low = serial.save_state().unwrap();
    serial.reset().unwrap();
    serial.restore_state(&low).unwrap();
    assert_eq!(*levels.lock(), [false, false, false]);
    write(&mut serial, 1, 1);
    serial.input_byte(b'x');
    let high = serial.save_state().unwrap();
    serial.restore_state(&high).unwrap();
    serial.reset().unwrap();
    serial.restore_state(&high).unwrap();
    assert_eq!(
        *levels.lock(),
        [false, false, false, true, true, false, true]
    );
    assert!(serial.restore_state(b"invalid").is_err());
    assert_eq!(
        *levels.lock(),
        [false, false, false, true, true, false, true]
    );
}

fn make() -> (Serial, Recording) {
    let rec = Recording::default();
    let serial = Serial::new(
        "serial-com1",
        SerialOut::new(Box::new(RecWriter(rec.clone()))),
        Arc::new(RecIrq(rec.clone())),
    );
    (serial, rec)
}

fn read(serial: &mut Serial, register: u64) -> u8 {
    let mut byte = [0];
    serial.read(register, &mut byte);
    byte[0]
}

fn write(serial: &mut Serial, register: u64, byte: u8) {
    serial.write(register, &[byte]);
}

fn configure(serial: &mut Serial, divisor: u16, lcr: u8, fcr: u8) {
    write(serial, 3, 0x80);
    write(serial, 0, divisor as u8);
    write(serial, 1, (divisor >> 8) as u8);
    write(serial, 3, lcr);
    write(serial, 2, fcr);
}

fn irq(rec: &Recording) -> bool {
    *rec.level.lock()
}

#[test]
fn qemu_socket_capture_transmit_status_and_fifo_overflow() {
    let capture: serde_json::Value =
        serde_json::from_str(include_str!("qemu-socket.json")).unwrap();
    let bytes = |value: &serde_json::Value| -> Vec<u8> {
        value
            .as_array()
            .unwrap()
            .iter()
            .map(|byte| byte.as_u64().unwrap() as u8)
            .collect()
    };
    let register = |value: &serde_json::Value| value.as_u64().unwrap() as u8;
    let (mut serial, rec) = make();
    configure(&mut serial, 1, 3, 7);
    write(&mut serial, 1, 2);
    let unblocked = &capture["tx_unblocked"];
    for byte in bytes(&unblocked["input"]) {
        write(&mut serial, 0, byte);
    }
    assert_eq!(*rec.bytes.lock(), bytes(&unblocked["immediate"]));
    assert_eq!(read(&mut serial, 5), register(&unblocked["lsr"]));
    assert_eq!(read(&mut serial, 2), register(&unblocked["iir"]));
    assert_eq!(
        read(&mut serial, 2),
        register(&unblocked["acknowledged_iir"])
    );

    rec.bytes.lock().clear();
    let blocked = &capture["tx_backpressure"];
    let filler = blocked["filler_written"].as_u64().unwrap() as usize;
    for _ in 1..filler {
        write(&mut serial, 0, b'a');
    }
    *rec.blocked.lock() = true;
    write(&mut serial, 0, b'a');
    assert_eq!(read(&mut serial, 5), register(&blocked["blocked_lsr"]));
    for byte in bytes(&blocked["markers_written"]) {
        write(&mut serial, 0, byte);
    }
    assert_eq!(read(&mut serial, 5), register(&blocked["full_lsr"]));
    assert_eq!(read(&mut serial, 2), register(&blocked["full_iir"]));
    *rec.blocked.lock() = false;
    serial.advance(CHARACTER_NS * 200);
    let mut expected = vec![b'a'; filler];
    expected.extend(bytes(&blocked["markers_received"]));
    assert_eq!(*rec.bytes.lock(), expected);
    assert_eq!(read(&mut serial, 5), register(&blocked["final_lsr"]));
    assert_eq!(read(&mut serial, 2), register(&blocked["final_iir"]));
}

#[test]
fn qemu_socket_capture_receive_trigger_and_timeout() {
    let capture: serde_json::Value =
        serde_json::from_str(include_str!("qemu-socket.json")).unwrap();
    let receive = &capture["rx"];
    let register = |name| receive[name].as_u64().unwrap() as u8;
    for (index, threshold) in receive["triggers"].as_array().unwrap().iter().enumerate() {
        let threshold = threshold.as_u64().unwrap() as usize;
        let (mut serial, _) = make();
        configure(&mut serial, 1, 3, 1 | ((index as u8 + 1) << 6));
        write(&mut serial, 1, 1);
        assert_eq!(serial.input(&vec![b'x'; threshold - 1]), threshold - 1);
        assert_eq!(read(&mut serial, 2), register("below_iir"));
        serial.input_byte(b'y');
        assert_eq!(read(&mut serial, 2), register("reached_iir"));
        for _ in 1..threshold {
            assert_eq!(read(&mut serial, 0), b'x');
        }
        assert_eq!(read(&mut serial, 0), b'y');
        assert_eq!(read(&mut serial, 2), register("empty_iir"));
    }
    let (mut serial, _) = make();
    configure(&mut serial, 1, 3, 0xc1);
    write(&mut serial, 1, 1);
    serial.input_byte(b'~');
    serial.advance(CHARACTER_NS * 3);
    assert_eq!(read(&mut serial, 2), register("below_iir"));
    serial.advance(receive["timeout_ns"].as_u64().unwrap() - CHARACTER_NS * 3);
    assert_eq!(read(&mut serial, 2), register("timeout_iir"));
    assert_eq!(read(&mut serial, 0), b'~');
    assert_eq!(read(&mut serial, 2), register("empty_iir"));
}

#[test]
fn qemu_socket_capture_receive_burst_and_nonfifo_backpressure() {
    let capture: serde_json::Value =
        serde_json::from_str(include_str!("qemu-socket.json")).unwrap();
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("agent.sock");
    let mut serial = socket_serial(&path);
    configure(&mut serial, 1, 3, 0xc1);
    write(&mut serial, 1, 1);
    let mut client = UnixStream::connect(&path).unwrap();
    let burst_count = capture["rx"]["burst_count"].as_u64().unwrap() as u8;
    let burst: Vec<_> = (0..burst_count).collect();
    client.write_all(&burst).unwrap();
    for byte in burst {
        assert_eq!(read(&mut serial, 5) & 3, 1);
        assert_eq!(read(&mut serial, 0), byte);
    }
    assert_eq!(read(&mut serial, 5), 0x60);
    write(&mut serial, 2, 0);
    let nonfifo: Vec<_> = capture["rx"]["nonfifo"]
        .as_array()
        .unwrap()
        .iter()
        .map(|byte| byte.as_u64().unwrap() as u8)
        .collect();
    client.write_all(&nonfifo).unwrap();
    for byte in nonfifo {
        assert_eq!(read(&mut serial, 5) & 3, 1);
        assert_eq!(read(&mut serial, 0), byte);
    }
    assert_eq!(read(&mut serial, 5), 0x60);
    assert_eq!(read(&mut serial, 2), 1);
}

#[test]
fn power_on_registers_match_the_16550a() {
    let (mut serial, rec) = make();
    assert_eq!(read(&mut serial, 1), 0);
    assert_eq!(read(&mut serial, 2), 1);
    assert_eq!(read(&mut serial, 3), 0);
    assert_eq!(read(&mut serial, 4), 0);
    assert_eq!(read(&mut serial, 5), 0x60);
    assert_eq!(read(&mut serial, 6), 0xb0);
    assert_eq!(read(&mut serial, 7), 0);
    assert!(!irq(&rec));
}

#[test]
fn writable_backend_transmits_without_advancing_guest_time() {
    let (mut serial, rec) = make();
    configure(&mut serial, 1, 3, 0);
    write(&mut serial, 0, b'x');
    assert_eq!(*rec.bytes.lock(), b"x");
    assert_eq!(read(&mut serial, 5), 0x60);
    serial.advance(CHARACTER_NS - 1);
    assert_eq!(*rec.bytes.lock(), b"x");
    serial.advance(1);
    assert_eq!(*rec.bytes.lock(), b"x");
    assert_eq!(read(&mut serial, 5), 0x60);
}

#[test]
fn output_reaches_the_log_in_order() {
    let (mut serial, rec) = make();
    configure(&mut serial, 1, 3, 1);
    for byte in b"hello" {
        write(&mut serial, 0, *byte);
    }
    serial.advance(CHARACTER_NS * 5);
    assert_eq!(*rec.bytes.lock(), b"hello");
}

#[test]
fn divisor_parity_and_stop_bits_change_receive_timeout() {
    for (divisor, lcr, ns) in [
        (1, 3, CHARACTER_NS),
        (12, 3, 1_041_667),
        (1, 0x1b, 95_487),
        (1, 0x1f, 104_167),
        (1, 4, 65_105),
        (0, 3, CHARACTER_NS),
    ] {
        let (mut serial, rec) = make();
        configure(&mut serial, divisor, lcr, 0xc1);
        write(&mut serial, 1, 1);
        serial.input_byte(b'x');
        write(&mut serial, 0, b'A');
        assert_eq!(*rec.bytes.lock(), b"A");
        serial.advance(ns * 4 - 1);
        assert!(!irq(&rec), "divisor={divisor} lcr={lcr:#x}");
        serial.advance(1);
        assert!(irq(&rec));
        assert_eq!(read(&mut serial, 2), 0xcc);
    }
}

#[test]
fn divisor_latches_do_not_transmit_or_change_interrupt_enable() {
    let (mut serial, rec) = make();
    write(&mut serial, 1, 5);
    write(&mut serial, 3, 0x80);
    write(&mut serial, 0, 0x34);
    write(&mut serial, 1, 0x12);
    assert_eq!(read(&mut serial, 0), 0x34);
    assert_eq!(read(&mut serial, 1), 0x12);
    write(&mut serial, 3, 3);
    assert_eq!(read(&mut serial, 1), 5);
    serial.advance(u64::MAX);
    assert!(rec.bytes.lock().is_empty());
}

#[test]
fn receive_interrupt_is_level_triggered_and_respects_ier() {
    let (mut serial, rec) = make();
    assert!(serial.input_byte(b'x'));
    assert!(!irq(&rec));
    write(&mut serial, 1, 1);
    assert!(irq(&rec));
    assert_eq!(read(&mut serial, 2), 4);
    assert!(irq(&rec));
    assert_eq!(read(&mut serial, 0), b'x');
    assert!(!irq(&rec));
    assert_eq!(read(&mut serial, 5), 0x60);
}

#[test]
fn receive_fifo_thresholds_and_character_timeout() {
    for (fcr, trigger) in [(1, 1), (0x41, 4), (0x81, 8), (0xc1, 14)] {
        let (mut serial, rec) = make();
        configure(&mut serial, 1, 3, fcr);
        write(&mut serial, 1, 1);
        for _ in 1..trigger {
            assert!(serial.input_byte(b'x'));
            assert!(!irq(&rec));
        }
        assert!(serial.input_byte(b'y'));
        assert!(irq(&rec));
        assert_eq!(read(&mut serial, 2), 0xc4);
        read(&mut serial, 0);
        if trigger > 1 {
            assert!(!irq(&rec));
            serial.advance(CHARACTER_NS * 4 - 1);
            assert!(!irq(&rec));
            serial.advance(1);
            assert!(irq(&rec));
            assert_eq!(read(&mut serial, 2), 0xcc);
            assert!(irq(&rec));
            read(&mut serial, 0);
            assert!(!irq(&rec));
        }
    }
}

#[test]
fn receive_backpressure_is_bounded_without_dropping_unaccepted_bytes() {
    let (mut serial, _) = make();
    assert_eq!(serial.input(b"ab"), 1);
    assert_eq!(serial.input(b"b"), 0);
    assert_eq!(read(&mut serial, 0), b'a');
    assert_eq!(serial.input(b"b"), 1);
    assert_eq!(read(&mut serial, 0), b'b');
    configure(&mut serial, 1, 3, 1);
    let bytes: Vec<u8> = (0..32).collect();
    assert_eq!(serial.input(&bytes), 16);
    assert_eq!(serial.input(&bytes[16..]), 0);
    for byte in &bytes[..16] {
        assert_eq!(read(&mut serial, 0), *byte);
    }
    assert_eq!(serial.input(&bytes[16..]), 16);
    for byte in &bytes[16..] {
        assert_eq!(read(&mut serial, 0), *byte);
    }
}

#[test]
fn thr_empty_interrupt_precedes_shift_register_empty() {
    let (mut serial, rec) = make();
    configure(&mut serial, 1, 3, 1);
    write(&mut serial, 1, 2);
    assert!(irq(&rec));
    assert_eq!(read(&mut serial, 2), 0xc2);
    assert!(!irq(&rec));
    *rec.blocked.lock() = true;
    write(&mut serial, 0, b'a');
    assert!(irq(&rec));
    assert_eq!(read(&mut serial, 5) & 0x60, 0x20);
    assert_eq!(read(&mut serial, 2), 0xc2);
    write(&mut serial, 0, b'b');
    assert!(!irq(&rec));
    assert_eq!(read(&mut serial, 5) & 0x60, 0);
    *rec.blocked.lock() = false;
    serial.advance(0);
    assert_eq!(read(&mut serial, 5) & 0x60, 0x60);
    assert_eq!(read(&mut serial, 2), 0xc2);
    assert!(!irq(&rec));
    assert_eq!(*rec.bytes.lock(), b"ab");
}

#[test]
fn full_transmit_fifo_keeps_only_its_bounded_contents() {
    let (mut serial, rec) = make();
    configure(&mut serial, 1, 3, 1);
    *rec.blocked.lock() = true;
    for byte in 0..18 {
        write(&mut serial, 0, byte);
    }
    assert_eq!(read(&mut serial, 5) & 0x60, 0);
    *rec.blocked.lock() = false;
    serial.advance(0);
    let expected: Vec<_> = std::iter::once(0).chain(2..18).collect();
    assert_eq!(*rec.bytes.lock(), expected);
    assert_eq!(read(&mut serial, 5) & 0x60, 0x60);
}

#[test]
fn host_backpressure_preserves_the_shift_register_and_fifo() {
    let (mut serial, rec) = make();
    configure(&mut serial, 1, 3, 1);
    *rec.blocked.lock() = true;
    for byte in b"abc" {
        write(&mut serial, 0, *byte);
    }
    serial.advance(u64::MAX);
    assert!(rec.bytes.lock().is_empty());
    assert_eq!(read(&mut serial, 5) & 0x60, 0);
    let state = serial.save_state().unwrap();
    serial.restore_state(&state).unwrap();
    *rec.blocked.lock() = false;
    serial.advance(0);
    assert_eq!(*rec.bytes.lock(), b"abc");
    assert_eq!(read(&mut serial, 5) & 0x60, 0x60);
}

#[test]
fn fifo_reset_discards_holding_bytes_but_not_the_in_flight_character() {
    let (mut serial, rec) = make();
    configure(&mut serial, 1, 3, 1);
    *rec.blocked.lock() = true;
    write(&mut serial, 0, b'a');
    write(&mut serial, 0, b'b');
    serial.input(b"rx");
    write(&mut serial, 2, 7);
    assert_eq!(read(&mut serial, 5), 0x20);
    *rec.blocked.lock() = false;
    serial.advance(0);
    assert_eq!(*rec.bytes.lock(), b"a");
    assert_eq!(read(&mut serial, 5), 0x60);
}

#[test]
fn disabling_fifo_returns_to_single_character_mode() {
    let (mut serial, _) = make();
    configure(&mut serial, 1, 3, 1);
    assert_eq!(serial.input(b"abc"), 3);
    write(&mut serial, 2, 0);
    assert_eq!(read(&mut serial, 5) & 1, 0);
    assert_eq!(serial.input(b"ab"), 1);
    assert_eq!(read(&mut serial, 2) & 0xc0, 0);
}

#[test]
fn loopback_connects_transmit_to_receive_immediately() {
    let (mut serial, rec) = make();
    configure(&mut serial, 1, 3, 0);
    write(&mut serial, 4, 0x10);
    write(&mut serial, 0, b'L');
    assert_eq!(read(&mut serial, 5) & 1, 1);
    assert_eq!(read(&mut serial, 0), b'L');
    assert!(rec.bytes.lock().is_empty());
}

#[test]
fn loopback_overrun_has_highest_interrupt_priority_and_lsr_clears_it() {
    let (mut serial, rec) = make();
    configure(&mut serial, 1, 3, 0);
    write(&mut serial, 4, 0x10);
    write(&mut serial, 1, 7);
    serial.input_byte(b'x');
    write(&mut serial, 0, b'y');
    serial.advance(CHARACTER_NS);
    assert!(irq(&rec));
    assert_eq!(read(&mut serial, 2), 6);
    assert_eq!(read(&mut serial, 5) & 3, 3);
    assert_eq!(read(&mut serial, 2), 4);
    assert_eq!(read(&mut serial, 0), b'x');
    assert_eq!(read(&mut serial, 2), 2);
    assert!(!irq(&rec));
}

#[test]
fn modem_loopback_routes_pins_and_latches_deltas_until_read() {
    let (mut serial, rec) = make();
    write(&mut serial, 1, 8);
    write(&mut serial, 4, 0x10);
    assert!(irq(&rec));
    assert_eq!(read(&mut serial, 6), 0x0b);
    assert!(!irq(&rec));
    write(&mut serial, 4, 0x1f);
    assert_eq!(read(&mut serial, 6), 0xfb);
    write(&mut serial, 4, 0x10);
    assert_eq!(read(&mut serial, 6), 0x0f);
    assert_eq!(read(&mut serial, 6), 0);
    write(&mut serial, 4, 0);
    assert_eq!(read(&mut serial, 6), 0xbb);
}

#[test]
fn accesses_outside_window_are_ignored_and_scratch_is_writable() {
    let (mut serial, rec) = make();
    write(&mut serial, 8, b'X');
    assert_eq!(read(&mut serial, 8), 0);
    serial.write(u64::MAX, &[1, 2]);
    serial.advance(u64::MAX);
    assert!(rec.bytes.lock().is_empty());
    write(&mut serial, 7, 0x5a);
    assert_eq!(read(&mut serial, 7), 0x5a);
}

#[test]
fn snapshot_round_trips_registers_fifos_interrupts_and_blocked_transmit() {
    let (mut serial, rec) = make();
    configure(&mut serial, 1, 3, 1);
    write(&mut serial, 1, 1);
    write(&mut serial, 4, 0x0b);
    write(&mut serial, 7, 0x42);
    serial.input(b"fifo");
    *rec.blocked.lock() = true;
    write(&mut serial, 0, b'a');
    write(&mut serial, 0, b'b');
    serial.advance(CHARACTER_NS);
    let state = serial.save_state().unwrap();
    serial.reset().unwrap();
    assert!(!irq(&rec));
    serial.restore_state(&state).unwrap();
    assert_eq!(read(&mut serial, 7), 0x42);
    assert_eq!(read(&mut serial, 4), 0x0b);
    assert_eq!(read(&mut serial, 3), 3);
    assert_eq!(read(&mut serial, 2), 0xc4);
    assert!(irq(&rec));
    for byte in b"fifo" {
        assert_eq!(read(&mut serial, 0), *byte);
    }
    assert!(!irq(&rec));
    serial.advance(0);
    assert!(rec.bytes.lock().is_empty());
    *rec.blocked.lock() = false;
    serial.advance(0);
    assert_eq!(*rec.bytes.lock(), b"ab");
}

#[test]
fn malformed_snapshot_is_rejected_without_mutation() {
    let (mut serial, _) = make();
    write(&mut serial, 7, 0x42);
    let good = serial.save_state().unwrap();
    let mut bad = good.clone();
    bad[0] ^= 1;
    assert!(serial.restore_state(&bad).is_err());
    assert!(serial.restore_state(&good[..good.len() - 1]).is_err());
    for (field, value) in [
        ("ier", serde_json::json!(16)),
        ("fcr", serde_json::json!(2)),
        ("tx", serde_json::json!([1, 2])),
        ("shift_ns", serde_json::json!(1)),
        ("timeout", serde_json::json!(true)),
    ] {
        let mut state: serde_json::Value =
            serde_json::from_slice(&good[SNAPSHOT_MAGIC.len()..]).unwrap();
        state[field] = value;
        let mut bytes = SNAPSHOT_MAGIC.to_vec();
        bytes.extend(serde_json::to_vec(&state).unwrap());
        assert!(serial.restore_state(&bytes).is_err(), "{field}");
        assert_eq!(serial.save_state().unwrap(), good);
    }
}

fn socket_serial(path: &Path) -> Serial {
    Serial::socket(
        "serial-socket",
        path,
        Arc::new(RecIrq(Recording::default())),
    )
    .unwrap()
}

#[test]
fn socket_connection_survives_reset_and_snapshot_load() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("agent.sock");
    let mut serial = socket_serial(&path);
    let mut client = UnixStream::connect(&path).unwrap();
    client
        .set_read_timeout(Some(Duration::from_secs(1)))
        .unwrap();
    configure(&mut serial, 1, 3, 1);
    client.write_all(b"hello").unwrap();
    serial.advance(0);
    let state = serial.save_state().unwrap();
    serial.reset().unwrap();
    configure(&mut serial, 1, 3, 1);
    client.write_all(b"x").unwrap();
    serial.advance(0);
    assert_eq!(read(&mut serial, 0), b'x');
    serial.restore_state(&state).unwrap();
    for byte in b"hello" {
        assert_eq!(read(&mut serial, 0), *byte);
    }
    for byte in b"reply" {
        write(&mut serial, 0, *byte);
    }
    serial.advance(CHARACTER_NS * 5);
    let mut reply = [0; 5];
    client.read_exact(&mut reply).unwrap();
    assert_eq!(&reply, b"reply");
    client.write_all(b"again").unwrap();
    serial.advance(0);
    for byte in b"again" {
        assert_eq!(read(&mut serial, 0), *byte);
    }
}

#[test]
fn socket_receive_applies_fifo_backpressure_and_accepts_reconnection() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("agent.sock");
    let mut serial = socket_serial(&path);
    configure(&mut serial, 1, 3, 1);
    let bytes: Vec<u8> = (0..64).collect();
    let mut client = UnixStream::connect(&path).unwrap();
    client.write_all(&bytes).unwrap();
    serial.advance(0);
    for byte in bytes {
        assert_eq!(read(&mut serial, 0), byte);
    }
    drop(client);
    serial.advance(0);
    let mut client = UnixStream::connect(&path).unwrap();
    client.write_all(b"z").unwrap();
    serial.advance(0);
    assert_eq!(read(&mut serial, 0), b'z');
}

#[test]
fn a_blocked_socket_keeps_accepted_transmit_bytes_until_the_client_reads() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("agent.sock");
    let mut serial = socket_serial(&path);
    configure(&mut serial, 1, 3, 1);
    let mut client = UnixStream::connect(&path).unwrap();
    client
        .set_read_timeout(Some(Duration::from_secs(1)))
        .unwrap();
    serial.advance(0);
    let mut out = serial.output();
    let mut padding = 0;
    loop {
        assert!(padding < 4 * 1024 * 1024);
        match out.write(&[0xaa; 8192]) {
            Ok(count) => padding += count,
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => break,
            other => panic!("unexpected socket write: {other:?}"),
        }
    }
    loop {
        assert!(padding < 4 * 1024 * 1024);
        match out.write(&[0xaa]) {
            Ok(1) => padding += 1,
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => break,
            other => panic!("unexpected socket write: {other:?}"),
        }
    }
    write(&mut serial, 0, b'a');
    write(&mut serial, 0, b'b');
    serial.advance(CHARACTER_NS * 10);
    assert_eq!(read(&mut serial, 5) & 0x60, 0);
    let mut drain = vec![0; padding];
    client.read_exact(&mut drain).unwrap();
    assert!(drain.iter().all(|byte| *byte == 0xaa));
    serial.advance(0);
    serial.advance(CHARACTER_NS);
    let mut reply = [0; 2];
    client.read_exact(&mut reply).unwrap();
    assert_eq!(&reply, b"ab");
    assert_eq!(read(&mut serial, 5) & 0x60, 0x60);
}

#[test]
fn socket_binding_preserves_existing_paths_and_drop_preserves_replacements() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("agent.sock");
    std::fs::write(&path, b"owned by someone else").unwrap();
    assert!(Serial::socket("test", &path, Arc::new(RecIrq(Recording::default()))).is_err());
    assert_eq!(std::fs::read(&path).unwrap(), b"owned by someone else");
    std::fs::remove_file(&path).unwrap();
    let serial = socket_serial(&path);
    let moved = directory.path().join("moved.sock");
    std::fs::rename(&path, &moved).unwrap();
    std::fs::write(&path, b"replacement").unwrap();
    drop(serial);
    assert_eq!(std::fs::read(&path).unwrap(), b"replacement");
}

#[test]
fn socket_path_is_released_after_last_output_handle_is_dropped() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("agent.sock");
    let serial = socket_serial(&path);
    let output = serial.output();
    drop(serial);
    assert!(path.exists());
    drop(output);
    assert!(!path.exists());
}
