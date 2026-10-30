use std::sync::{Arc, Mutex};

use vm_memory::{Bytes, GuestAddress};

use crate::{BusDevice, GuestMemory, MsiMessage, MsiSink, PciDevice, Snapshot};

use super::{PointerEvent, XhciController, ring::Trb};

#[derive(Default)]
struct Interrupts(Mutex<Vec<MsiMessage>>);
impl MsiSink for Interrupts {
    fn signal(&self, message: MsiMessage) {
        self.0.lock().unwrap().push(message);
    }
}

struct Driver {
    hc: XhciController,
    mem: GuestMemory,
    irq: Arc<Interrupts>,
    command: u64,
    event: u64,
    control: u64,
    interrupt: u64,
    slot: u32,
}

impl Driver {
    fn new() -> Self {
        let mem = GuestMemory::from_ranges(&[(GuestAddress(0), 0x100000)]).unwrap();
        Self::with_memory(mem)
    }
    fn with_memory(mem: GuestMemory) -> Self {
        let irq = Arc::new(Interrupts::default());
        let hc = XhciController::new("xhci0", mem.clone(), irq.clone(), None);
        Self {
            hc,
            mem,
            irq,
            command: 0x1000,
            event: 0x3000,
            control: 0x7000,
            interrupt: 0x8000,
            slot: 0,
        }
    }
    fn write(&mut self, off: u64, v: u32) {
        self.hc.bar_write(0, off, &v.to_le_bytes());
    }
    fn read(&mut self, off: u64) -> u32 {
        let mut b = [0; 4];
        self.hc.bar_read(0, off, &mut b);
        u32::from_le_bytes(b)
    }
    fn mem32(&self, addr: u64, value: u32) {
        self.mem
            .write_slice(&value.to_le_bytes(), GuestAddress(addr))
            .unwrap();
    }
    fn mem64(&self, addr: u64, value: u64) {
        self.mem
            .write_slice(&value.to_le_bytes(), GuestAddress(addr))
            .unwrap();
    }
    fn trb(&self, addr: u64, p: u64, status: u32, ctrl: u32) {
        Trb {
            parameter: p,
            status,
            control: ctrl | 1,
        }
        .write(&self.mem, addr)
        .unwrap();
    }
    fn start(&mut self) {
        self.hc.config_write(1, 0, &6u16.to_le_bytes());
        self.write(0x40, 2);
        assert_eq!(self.read(0x40), 0);
        assert_eq!(self.read(0x44), 1);
        self.hc.config_write(0x90 / 4, 3, &[0x80]);
        self.write(0x3000, 0xfee00000);
        self.write(0x3008, 0x40);
        self.write(0x300c, 0);
        self.mem64(0x2000, 0x3000);
        self.mem32(0x2008, 256);
        self.write(0x1028, 1);
        self.write(0x1030, 0x2000);
        self.write(0x1038, 0x3000);
        self.write(0x1020, 2);
        self.write(0x58, 0x1001);
        self.write(0x70, 0x4000);
        self.write(0x78, 64);
        self.write(0x40, 5);
        assert_eq!(self.read(0x44) & 1, 0);
    }
    fn event(&mut self, kind: u32) -> Trb {
        let t = Trb::read(&self.mem, self.event).unwrap();
        assert_eq!(t.control & 1, 1, "event missing at {:#x}", self.event);
        assert_eq!(t.kind(), kind);
        self.event += 16;
        self.write(0x1020, 3);
        self.write(0x44, 8);
        self.write(0x1038, self.event as u32 | 8);
        t
    }
    fn command(&mut self, kind: u32, p: u64, flags: u32) -> Trb {
        self.trb(self.command, p, 0, (kind << 10) | flags);
        self.command += 16;
        self.write(0x2000, 0);
        self.event(33)
    }
    fn enumerate(&mut self) {
        self.start();
        self.write(0x480, 0x210);
        assert_eq!(self.event(34).parameter, 5 << 24);
        let t = self.command(9, 0, 0);
        assert_eq!(t.status >> 24, 1);
        self.slot = t.control >> 24;
        self.mem64(0x4000 + u64::from(self.slot) * 8, 0x5000);
        self.mem32(0x6004, 3);
        self.mem32(0x6020, (3 << 20) | (1 << 27));
        self.mem32(0x6024, 5 << 16);
        self.mem32(0x6044, (4 << 3) | (64 << 16));
        self.mem64(0x6048, 0x7001);
        assert_eq!(self.command(11, 0x6000, self.slot << 24).status >> 24, 1);
        let mut ctx = [0; 16];
        self.mem.read_slice(&mut ctx, GuestAddress(0x5000)).unwrap();
        assert_eq!(
            u32::from_le_bytes(ctx[12..].try_into().unwrap()),
            (2 << 27) | self.slot
        );
    }
    fn control(&mut self, setup: [u8; 8], buffer_len: u32) -> Vec<Trb> {
        let data = setup[0] & 0x80 != 0;
        self.trb(
            self.control,
            u64::from_le_bytes(setup),
            8,
            (2 << 10)
                | 64
                | if buffer_len > 0 {
                    if data { 3 << 16 } else { 2 << 16 }
                } else {
                    0
                },
        );
        self.control += 16;
        if buffer_len > 0 {
            self.trb(
                self.control,
                0x9000,
                buffer_len,
                (3 << 10) | 4 | if data { 1 << 16 } else { 0 },
            );
            self.control += 16;
        }
        self.trb(
            self.control,
            0,
            0,
            (4 << 10) | 32 | if data { 0 } else { 1 << 16 },
        );
        self.control += 16;
        self.write(0x2000 + u64::from(self.slot) * 4, 1);
        let mut events = Vec::new();
        if data && buffer_len > u32::from(u16::from_le_bytes([setup[6], setup[7]])) {
            events.push(self.event(32));
        }
        events.push(self.event(32));
        events
    }
    fn configure(&mut self) {
        self.control([0, 9, 1, 0, 0, 0, 0, 0], 0);
        self.mem32(0x6000, 0);
        self.mem32(0x6004, 9);
        self.mem32(0x6020, (3 << 20) | (3 << 27));
        self.mem32(0x6084, (7 << 3) | (8 << 16));
        self.mem64(0x6088, 0x8001);
        assert_eq!(self.command(12, 0x6000, self.slot << 24).status >> 24, 1);
    }
    fn queue_report(&mut self) {
        self.trb(self.interrupt, 0xa000, 8, (1 << 10) | 32 | 4);
        self.interrupt += 16;
        self.write(0x2000 + u64::from(self.slot) * 4, 3);
    }
    fn report(&self) -> [u8; 6] {
        let mut b = [0; 6];
        self.mem.read_slice(&mut b, GuestAddress(0xa000)).unwrap();
        b
    }
}

#[test]
fn p4_enumerate_descriptors_and_receive_tablet_input() {
    let mut d = Driver::new();
    d.enumerate();
    d.control([0x80, 6, 0, 1, 0, 0, 18, 0], 18);
    let mut descriptor = [0; 18];
    d.mem
        .read_slice(&mut descriptor, GuestAddress(0x9000))
        .unwrap();
    assert_eq!(&descriptor[8..12], &[0x27, 6, 1, 0]);
    d.control([0x81, 6, 0, 0x22, 0, 0, 74, 0], 74);
    let mut report_desc = [0; 74];
    d.mem
        .read_slice(&mut report_desc, GuestAddress(0x9000))
        .unwrap();
    assert_eq!(report_desc, super::TABLET_REPORT_DESCRIPTOR);
    d.configure();
    d.queue_report();
    assert_eq!(
        Trb::read(&d.mem, d.event).unwrap().control,
        0,
        "NAK must not complete the queued report"
    );
    d.hc.inject_pointer(PointerEvent {
        x: 32767,
        y: 1234,
        buttons: 0x19,
        wheel: -3,
    })
    .unwrap();
    let event = d.event(32);
    assert_eq!(event.status >> 24, 13);
    assert_eq!(event.status & 0xff_ffff, 2);
    assert_eq!((event.control >> 16) & 31, 3);
    assert_eq!(d.report(), [0x19, 255, 127, 0xd2, 4, 253]);
    let irq = d.irq.0.lock().unwrap();
    assert!(irq.len() >= 5);
    assert!(
        irq.iter()
            .all(|m| m.address == 0xfee00000 && m.data == 0x40)
    );
}

#[test]
fn pci_identity_bar_probe_and_readonly_registers() {
    let mut d = Driver::new();
    assert_eq!(d.hc.config_read(0), 0x000d1b36);
    assert_eq!(d.hc.config_read(2), 0x0c033001);
    assert_eq!(d.hc.config_read(11), 0x11001af4);
    d.hc.config_write(4, 0, &u32::MAX.to_le_bytes());
    assert_eq!(d.hc.config_read(4), 0xffffc004);
    d.hc.config_write(5, 0, &u32::MAX.to_le_bytes());
    assert_eq!(d.hc.config_read(5), u32::MAX);
    d.write(0, 0);
    assert_eq!(d.read(0), 0x01000040);
    assert_eq!(d.read(4), (8 << 24) | (16 << 8) | 64);
    assert_eq!((d.hc.config_read(0x90 / 4) >> 16) & 0x7ff, 15);
    assert_eq!(d.hc.config_read(0x100 / 4), 0);
    assert_eq!(d.hc.config_read(0xffc / 4), 0);
    assert_eq!(d.hc.config_read(0x1000 / 4), u32::MAX);
    d.hc.config_write(1, 0, &0x0107u16.to_le_bytes());
    assert_eq!(d.hc.config_read(1) & 0x0107, 0x0107);
    assert_eq!(d.read(0x28), 0x405);
    assert_eq!(d.read(0x38), 0x401);
    d.hc.write(u64::MAX, &[1; 16]);
    let mut bytes = [0; 8];
    d.hc.read(u64::MAX, &mut bytes);
    assert_eq!(bytes, [0; 8]);
}

#[test]
fn snapshot_restores_queued_input_and_pending_interrupt_transfer() {
    let mut d = Driver::new();
    d.enumerate();
    d.configure();
    d.hc.inject_pointer(PointerEvent {
        x: 10,
        y: 20,
        buttons: 1,
        wheel: 2,
    })
    .unwrap();
    let state = d.hc.save_state().unwrap();
    let ram = {
        let mut b = vec![0; 0x100000];
        d.mem.read_slice(&mut b, GuestAddress(0)).unwrap();
        b
    };
    let command = d.command;
    let event = d.event;
    let control = d.control;
    let interrupt = d.interrupt;
    d.queue_report();
    d.event(32);
    assert_eq!(d.report(), [1, 10, 0, 20, 0, 2]);
    d.hc.inject_pointer(PointerEvent::default()).unwrap();
    d.mem.write_slice(&ram, GuestAddress(0)).unwrap();
    d.hc.restore_state(&state).unwrap();
    d.command = command;
    d.event = event;
    d.control = control;
    d.interrupt = interrupt;
    d.queue_report();
    d.event(32);
    assert_eq!(d.report(), [1, 10, 0, 20, 0, 2]);
    d.queue_report();
    let state = d.hc.save_state().unwrap();
    d.hc.restore_state(&state).unwrap();
    d.hc.inject_pointer(PointerEvent {
        x: 21,
        ..PointerEvent::default()
    })
    .unwrap();
    d.event(32);
    assert_eq!(d.report(), [0, 21, 0, 0, 0, 0]);
    assert!(d.hc.restore_state(&state[..state.len() - 1]).is_err());
}

#[test]
fn msix_masks_latch_pba_and_unmask_sends_once() {
    let mut d = Driver::new();
    d.start();
    d.write(0x300c, 1);
    d.trb(d.command, 0, 0, 23 << 10);
    d.command += 16;
    d.write(0x2000, 0);
    assert!(d.irq.0.lock().unwrap().is_empty());
    assert_eq!(d.read(0x3800), 1);
    d.write(0x300c, 0);
    assert_eq!(d.irq.0.lock().unwrap().len(), 1);
    assert_eq!(d.read(0x3800), 0);
    d.write(0x300c, 0);
    d.hc.poll();
    assert_eq!(d.irq.0.lock().unwrap().len(), 1);
    d.event(33);
    d.hc.config_write(0x90 / 4, 3, &[0xc0]);
    d.trb(d.command, 0, 0, 23 << 10);
    d.command += 16;
    d.write(0x2000, 0);
    assert_eq!(d.irq.0.lock().unwrap().len(), 1);
    assert_eq!(d.read(0x3800), 1);
    let snapshot = d.hc.save_state().unwrap();
    d.hc.restore_state(&snapshot).unwrap();
    assert_eq!(d.irq.0.lock().unwrap().len(), 1);
    d.hc.config_write(0x90 / 4, 3, &[0x80]);
    assert_eq!(d.irq.0.lock().unwrap().len(), 2);
    d.event(33);
    d.write(0x40, 2);
    assert_eq!(
        d.read(0x3000),
        0xfee00000,
        "HC reset must preserve the PCI MSI-X table"
    );
}

#[test]
fn command_links_toggle_cycle_and_ignore_unowned_trbs() {
    let mut d = Driver::new();
    d.start();
    d.trb(0x1000, 0, 0, 23 << 10);
    d.trb(0x1010, 0x1100, 0, (6 << 10) | 2);
    Trb {
        parameter: 0,
        status: 0,
        control: 23 << 10,
    }
    .write(&d.mem, 0x1100)
    .unwrap();
    d.trb(0x1110, 0, 0, 23 << 10);
    d.write(0x2000, 0);
    assert_eq!(d.event(33).parameter, 0x1000);
    assert_eq!(d.event(33).parameter, 0x1100);
    assert_eq!(Trb::read(&d.mem, d.event).unwrap().control, 0);
    d.write(0x40, 0);
    let mut d = Driver::new();
    d.start();
    Trb {
        parameter: 0,
        status: 0,
        control: 23 << 10,
    }
    .write(&d.mem, 0x1000)
    .unwrap();
    d.write(0x2000, 0);
    assert_eq!(Trb::read(&d.mem, 0x3000).unwrap().control, 0);
}

#[test]
fn event_ring_wraps_segments_and_preserves_ownership_when_full() {
    let mut d = Driver::new();
    d.start();
    d.mem32(0x2008, 16);
    d.mem64(0x2010, 0x3400);
    d.mem32(0x2018, 16);
    d.write(0x1028, 2);
    d.write(0x1030, 0x2000);
    for n in 0..40 {
        d.trb(0x1000 + n * 16, 0, 0, 23 << 10);
    }
    d.write(0x2000, 0);
    assert_eq!(
        Trb::read(&d.mem, 0x34f0).unwrap().control,
        0,
        "full ring must leave its reserved entry untouched"
    );
    for n in 0..40u64 {
        let index = n % 32;
        let addr = if index < 16 {
            0x3000 + index * 16
        } else {
            0x3400 + (index - 16) * 16
        };
        let t = Trb::read(&d.mem, addr).unwrap();
        assert_eq!(t.kind(), 33);
        assert_eq!(t.control & 1, u32::from(n < 32));
        assert_eq!(t.parameter, 0x1000 + n * 16);
        let next = (index + 1) % 32;
        let ptr = if next < 16 {
            0x3000 + next * 16
        } else {
            0x3400 + (next - 16) * 16
        };
        d.write(0x1020, 3);
        d.write(0x1038, ptr as u32 | 8);
        d.hc.poll();
    }
}

#[test]
fn scatter_gather_link_and_event_data_deliver_one_report() {
    let mut d = Driver::new();
    d.enumerate();
    d.configure();
    d.trb(0x8000, 0xa000, 2, (1 << 10) | 16);
    d.trb(0x8010, 0x8100, 0, 6 << 10);
    d.trb(0x8100, 0xb000, 4, (1 << 10) | 16);
    d.trb(0x8110, 0x123456789abcdef0, 0, (7 << 10) | 32);
    d.write(0x2000 + u64::from(d.slot) * 4, 3);
    d.hc.inject_pointer(PointerEvent {
        x: 0x1234,
        y: 0x5678,
        buttons: 2,
        wheel: 3,
    })
    .unwrap();
    let t = d.event(32);
    assert_eq!(t.status, (1 << 24) | 6);
    assert_eq!(t.control & 4, 4);
    assert_eq!(t.parameter, 0x123456789abcdef0);
    let mut first = [0; 2];
    let mut second = [0; 4];
    d.mem.read_slice(&mut first, GuestAddress(0xa000)).unwrap();
    d.mem.read_slice(&mut second, GuestAddress(0xb000)).unwrap();
    assert_eq!(first, [2, 0x34]);
    assert_eq!(second, [0x12, 0x78, 0x56, 3]);
}

#[test]
fn stall_reset_endpoint_set_dequeue_stop_and_deconfigure() {
    let mut d = Driver::new();
    d.enumerate();
    d.configure();
    d.control([0x80, 0xff, 0, 0, 0, 0, 0, 0], 0);
    // The unknown request stalls EP0 and reports the offending Setup TRB.
    let state = d.mem.read_obj::<u32>(GuestAddress(0x5020)).unwrap();
    assert_eq!(u32::from_le(state) & 7, 2);
    assert_eq!(d.command(14, 0, (d.slot << 24) | (1 << 16)).status >> 24, 1);
    assert_eq!(
        d.command(16, d.control | 1, (d.slot << 24) | (1 << 16))
            .status
            >> 24,
        1
    );
    d.control([0x80, 8, 0, 0, 0, 0, 1, 0], 1);
    assert_eq!(d.command(15, 0, (d.slot << 24) | (3 << 16)).status >> 24, 1);
    assert_eq!(
        d.command(16, 0x8201, (d.slot << 24) | (3 << 16)).status >> 24,
        1
    );
    assert_eq!(
        d.command(12, u64::MAX, (d.slot << 24) | 0x200).status >> 24,
        1,
        "deconfigure ignores the input pointer"
    );
    assert_eq!(d.command(10, 0, d.slot << 24).status >> 24, 1);
    assert_eq!(d.command(17, 0, d.slot << 24).status >> 24, 11);
}

#[test]
fn invalid_dma_and_cyclic_links_halt_without_host_panics() {
    let mut d = Driver::new();
    d.start();
    d.write(0x40, 0);
    d.write(0x58, 0xfffff001);
    d.write(0x40, 5);
    d.write(0x2000, 0);
    assert_ne!(d.read(0x44) & 0x1001, 0);
    assert_eq!(d.read(0x40) & 1, 0);
    d.write(0x40, 2);
    assert_eq!(d.read(0x44), 1);
    let mut d = Driver::new();
    d.start();
    d.trb(0x1000, 0x1000, 0, 6 << 10);
    d.write(0x2000, 0);
    assert_ne!(d.read(0x44) & 0x1000, 0);
}

#[test]
fn slot_limit_bad_commands_and_snapshot_topology_are_rejected() {
    let mut d = Driver::new();
    d.start();
    d.write(0x78, 1);
    assert_eq!(d.command(9, 0, 0).status >> 24, 1);
    assert_eq!(d.command(9, 0, 0).status >> 24, 9);
    assert_eq!(d.command(10, 0, 65 << 24).status >> 24, 11);
    assert_eq!(d.command(63, 0, 1 << 24).status >> 24, 5);
    let state = d.hc.save_state().unwrap();
    let tablet = d.hc.detach(5).unwrap();
    assert!(d.hc.restore_state(&state).is_err());
    assert!(d.hc.attach(1, Box::new(super::Tablet::default())).is_err());
    d.hc.attach(5, tablet).unwrap();
    d.hc.restore_state(&state).unwrap();
    assert!(d.hc.detach(0).is_err());
    assert!(d.hc.detach(9).is_err());
}

#[test]
fn command_and_event_dma_above_four_gib() {
    let high = 1u64 << 32;
    let mem =
        GuestMemory::from_ranges(&[(GuestAddress(0), 0x100000), (GuestAddress(high), 0x100000)])
            .unwrap();
    let mut d = Driver::with_memory(mem);
    d.start();
    d.command += high;
    d.event += high;
    d.mem64(high + 0x2000, high + 0x3000);
    d.mem32(high + 0x2008, 256);
    d.write(0x1030, 0x2000);
    d.write(0x1034, 1);
    d.write(0x103c, 1);
    d.write(0x1038, 0x3000);
    d.write(0x58, 0x1001);
    d.write(0x5c, 1);
    let event = d.command(9, 0, 0);
    assert_eq!(event.parameter, high + 0x1000);
    assert_eq!(event.status >> 24, 1);
    d.write(0x70, 0x4000);
    d.write(0x74, 1);
    d.mem64(high + 0x4008, high + 0x5000);
    d.mem32(high + 0x6004, 3);
    d.mem32(high + 0x6020, (3 << 20) | (1 << 27));
    d.mem32(high + 0x6024, 5 << 16);
    d.mem32(high + 0x6044, (4 << 3) | (64 << 16));
    d.mem64(high + 0x6048, high + 0x7001);
    d.write(0x480, 0x210);
    d.event(34);
    assert_eq!(d.command(11, high + 0x6000, 1 << 24).status >> 24, 1);
    let ctx = d.mem.read_obj::<u32>(GuestAddress(high + 0x500c)).unwrap();
    assert_eq!(u32::from_le(ctx), (2 << 27) | 1);
    d.trb(
        high + 0x7000,
        u64::from_le_bytes([0x80, 6, 0, 1, 0, 0, 18, 0]),
        8,
        (2 << 10) | 64 | (3 << 16),
    );
    d.trb(high + 0x7010, high + 0x9000, 18, (3 << 10) | (1 << 16));
    d.trb(high + 0x7020, 0, 0, (4 << 10) | 32);
    d.write(0x2004, 1);
    assert_eq!(d.event(32).status >> 24, 1);
    let mut bytes = [0; 18];
    d.mem
        .read_slice(&mut bytes, GuestAddress(high + 0x9000))
        .unwrap();
    assert_eq!(&bytes[8..12], &[0x27, 6, 1, 0]);
}

#[test]
fn stopped_nak_reports_transfer_before_command_completion() {
    let mut d = Driver::new();
    d.enumerate();
    d.configure();
    d.queue_report();
    d.trb(d.command, 0, 0, (15 << 10) | (d.slot << 24) | (3 << 16));
    d.command += 16;
    d.write(0x2000, 0);
    let stopped = d.event(32);
    assert_eq!(stopped.status >> 24, 26);
    assert_eq!(stopped.parameter, 0x8000);
    assert_eq!(d.event(33).status >> 24, 1);
    assert_eq!(
        d.command(16, 0x8101, (d.slot << 24) | (3 << 16)).status >> 24,
        1
    );
    d.trb(0x8100, 0xa000, 6, (1 << 10) | 32);
    d.write(0x2000 + u64::from(d.slot) * 4, 3);
    d.hc.inject_pointer(PointerEvent {
        x: 42,
        ..PointerEvent::default()
    })
    .unwrap();
    assert_eq!(d.event(32).status >> 24, 1);
    assert_eq!(d.report(), [0, 42, 0, 0, 0, 0]);
}

#[test]
fn truncated_snapshot_is_atomic_and_reset_clears_controller_state() {
    let mut d = Driver::new();
    d.enumerate();
    d.configure();
    d.write(0x40, 0);
    let snapshot = d.hc.save_state().unwrap();
    for len in [0, 1, 7, 8, 15, 16, 100, 2000, snapshot.len() - 1] {
        assert!(d.hc.restore_state(&snapshot[..len]).is_err());
        assert_eq!(d.hc.save_state().unwrap(), snapshot);
    }
    d.write(0x40, 2);
    assert_eq!(d.read(0x78), 0);
    assert_eq!(d.read(0x58), 0);
    assert_eq!(d.read(0x1030), 0);
    assert_eq!(d.read(0x1020), 0);
}

#[test]
fn tablet_class_requests_idle_and_snapshot_preserve_input() {
    use super::{SetupPacket, Tablet, UsbDevice, UsbTransfer};
    let mut t = Tablet::default();
    let setup = |b| SetupPacket::from_bytes(b);
    assert!(matches!(
        t.control(setup([0, 9, 1, 0, 0, 0, 0, 0]), &[]),
        UsbTransfer::Ack(_)
    ));
    assert!(matches!(
        t.control(setup([0x21, 10, 0, 1, 0, 0, 0, 0]), &[]),
        UsbTransfer::Ack(_)
    ));
    assert!(matches!(t.control(setup([0xa1,2,0,0,0,0,1,0]),&[]),UsbTransfer::Ack(b) if b==[1]));
    t.inject_pointer(PointerEvent {
        x: u16::MAX,
        y: 0x1234,
        buttons: u8::MAX,
        wheel: i8::MIN,
    })
    .unwrap();
    let state = t.save_state().unwrap();
    assert!(matches!(t.transfer(0x81,&[],8),UsbTransfer::Ack(b) if b==[31,255,127,0x34,0x12,129]));
    t.restore_state(&state).unwrap();
    assert!(matches!(t.transfer(0x81,&[],8),UsbTransfer::Ack(b) if b==[31,255,127,0x34,0x12,129]));
    std::thread::sleep(std::time::Duration::from_millis(6));
    assert!(matches!(t.transfer(0x81,&[],8),UsbTransfer::Ack(b) if b==[31,255,127,0x34,0x12,0]));
    assert!(matches!(
        t.control(setup([2, 3, 0, 0, 0x81, 0, 0, 0]), &[]),
        UsbTransfer::Ack(_)
    ));
    assert!(matches!(t.transfer(0x81, &[], 8), UsbTransfer::Stall));
    t.reset();
    assert!(matches!(t.transfer(0x81, &[], 8), UsbTransfer::Nak));
}

#[test]
fn msix_acknowledgement_does_not_reassert_an_empty_event_ring() {
    let mut d = Driver::new();
    d.start();
    for command in 0..2 {
        d.trb(d.command, 0, 0, 23 << 10);
        d.command += 16;
        d.write(0x2000, 0);
        assert_eq!(d.irq.0.lock().unwrap().len(), command + 1);
        assert_eq!(d.read(0x1020) & 1, 0);
        d.event += 16;
        d.write(0x44, 8);
        d.write(0x1038, d.event as u32 | 8);
        d.hc.poll();
        assert_eq!(d.irq.0.lock().unwrap().len(), command + 1);
        assert_eq!(d.read(0x1038) & 8, 0);
    }
}

#[test]
fn draining_events_added_while_busy_does_not_latch_a_phantom_interrupt() {
    let mut d = Driver::new();
    d.start();
    for _ in 0..2 {
        d.trb(d.command, 0, 0, 23 << 10);
        d.command += 16;
        d.write(0x2000, 0);
    }
    assert_eq!(d.irq.0.lock().unwrap().len(), 1);
    d.event += 32;
    d.write(0x44, 8);
    d.write(0x1038, d.event as u32 | 8);
    assert_eq!(d.irq.0.lock().unwrap().len(), 1);
    assert_eq!(d.read(0x1038) & 8, 0);
    d.trb(d.command, 0, 0, 23 << 10);
    d.write(0x2000, 0);
    assert_eq!(d.irq.0.lock().unwrap().len(), 2);
}

#[test]
fn secondary_interrupter_routes_tablet_dma_masks_and_snapshot_independently() {
    let mut d = Driver::new();
    d.enumerate();
    d.configure();
    let index = 15;
    let runtime = 0x1020 + index * 32;
    let vector = 0x3000 + index * 16;
    d.mem64(0xb000, 0xc000);
    d.mem32(0xb008, 256);
    d.write(runtime + 8, 1);
    d.write(runtime + 16, 0xb000);
    d.write(runtime + 24, 0xc000);
    d.write(runtime, 2);
    d.write(vector, 0xfee01000);
    d.write(vector + 8, 0x55);
    d.trb(
        d.interrupt,
        0xa000,
        8 | ((index as u32) << 22),
        (1 << 10) | 32,
    );
    d.hc.inject_pointer(PointerEvent {
        x: 123,
        y: 456,
        buttons: 1,
        wheel: 0,
    })
    .unwrap();
    let before = d.irq.0.lock().unwrap().len();
    d.write(0x2000 + u64::from(d.slot) * 4, 3);
    assert_eq!(Trb::read(&d.mem, 0xc000).unwrap().kind(), 32);
    assert_eq!(d.read(0x3800), 1 << index);
    assert_eq!(d.irq.0.lock().unwrap().len(), before);
    let saved = d.hc.save_state().unwrap();
    d.hc.restore_state(&saved).unwrap();
    d.write(vector + 12, 0);
    let messages = d.irq.0.lock().unwrap();
    assert_eq!(messages.len(), before + 1);
    assert_eq!(
        messages.last(),
        Some(&MsiMessage {
            address: 0xfee01000,
            data: 0x55
        })
    );
    drop(messages);
    assert_eq!(d.read(0x3800), 0);
    assert_eq!(d.read(runtime + 24) & 8, 8);
    d.write(0x44, 8);
    d.write(runtime + 24, 0xc018);
    assert_eq!(d.read(runtime + 24) & 8, 0);
    assert_eq!(d.read(0x1020) & 1, 0);
}

#[test]
fn interrupt_moderation_delays_delivery_until_counter_expires() {
    let mut d = Driver::new();
    d.start();
    d.write(0x1024, (40000 << 16) | 40000);
    d.trb(d.command, 0, 0, 23 << 10);
    d.write(0x2000, 0);
    assert!(d.irq.0.lock().unwrap().is_empty());
    assert!(d.read(0x1024) >> 16 > 0);
    std::thread::sleep(std::time::Duration::from_millis(12));
    d.hc.poll();
    assert_eq!(d.irq.0.lock().unwrap().len(), 1);
    d.event(33);
    // A driver can clear the live counter without changing the interval.
    d.hc.bar_write(0, 0x1026, &[0, 0]);
    assert_eq!(d.read(0x1024) & 0xffff, 40000);
    assert_eq!(d.read(0x1024) >> 16, 0);
}
