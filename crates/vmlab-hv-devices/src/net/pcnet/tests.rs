use super::*;
use std::sync::atomic::{AtomicBool, Ordering};

#[derive(Default)]
struct Irq(AtomicBool);
impl IrqLine for Irq {
    fn set_level(&self, high: bool) {
        self.0.store(high, Ordering::SeqCst);
    }
}
const MAC: [u8; 6] = [0x52, 0x54, 0, 1, 2, 3];
fn fixture() -> (Pcnet, GuestMemory, Arc<Irq>) {
    let mem = GuestMemory::from_ranges(&[(GuestAddress(0), 0x40000)]).unwrap();
    let irq = Arc::new(Irq::default());
    let mut nic = Pcnet::new("nic", MAC, mem.clone(), irq.clone());
    nic.config_write(1, 0, &7u16.to_le_bytes());
    nic.set_link(true);
    (nic, mem, irq)
}
fn csr(nic: &mut Pcnet, index: u16, value: u16) {
    let width = if nic.state.dword { 4 } else { 2 };
    nic.bar_write(
        0,
        if width == 4 { 0x14 } else { 0x12 },
        &u32::from(index).to_le_bytes()[..width],
    );
    nic.bar_write(0, 0x10, &u32::from(value).to_le_bytes()[..width]);
}
fn get(nic: &mut Pcnet, index: u16) -> u16 {
    let width = if nic.state.dword { 4 } else { 2 };
    nic.bar_write(
        0,
        if width == 4 { 0x14 } else { 0x12 },
        &u32::from(index).to_le_bytes()[..width],
    );
    let mut data = [0; 4];
    nic.bar_read(1, 0x10, &mut data[..width]);
    le16(&data, 0)
}
fn init(nic: &mut Pcnet, mem: &GuestMemory, style: u16) {
    csr(nic, 58, style);
    let mut block = [0; 28];
    if style == 0 {
        block[2..8].copy_from_slice(&MAC);
        block[16..20].copy_from_slice(&(0x2000u32 | 1 << 29).to_le_bytes());
        block[20..24].copy_from_slice(&(0x3000u32 | 1 << 29).to_le_bytes());
    } else {
        block[2] = 0x10;
        block[3] = 0x10;
        block[4..10].copy_from_slice(&MAC);
        block[20..24].copy_from_slice(&0x2000u32.to_le_bytes());
        block[24..28].copy_from_slice(&0x3000u32.to_le_bytes());
    }
    mem.write_slice(&block, GuestAddress(0x1000)).unwrap();
    csr(nic, 1, 0x1000);
    csr(nic, 2, 0);
    csr(nic, 0, 0x41);
    nic.service(0, &mut Frames::default());
    assert_ne!(get(nic, 0) & IDON, 0);
    csr(nic, 0, 0x142);
    assert_eq!(get(nic, 0) & 0x30, 0x30);
}
fn desc(nic: &Pcnet, mem: &GuestMemory, address: u64, buffer: u32, flags: u16, length: u16) {
    let mut bytes = [0; 16];
    if nic.wide() {
        let offset = if nic.style() == 3 { 8 } else { 0 };
        bytes[offset..offset + 4].copy_from_slice(&buffer.to_le_bytes());
        bytes[4..6].copy_from_slice(&length.wrapping_neg().to_le_bytes());
        bytes[6..8].copy_from_slice(&flags.to_le_bytes());
    } else {
        bytes[..4].copy_from_slice(&(buffer | u32::from(flags) << 16).to_le_bytes());
        bytes[4..6].copy_from_slice(&length.wrapping_neg().to_le_bytes());
    }
    mem.write_slice(
        &bytes[..if nic.wide() { 16 } else { 8 }],
        GuestAddress(address),
    )
    .unwrap();
}
fn packet() -> Vec<u8> {
    let mut packet = vec![0x55; 100];
    packet[..6].copy_from_slice(&MAC);
    packet[12..14].copy_from_slice(&[8, 6]);
    packet
}

#[test]
fn all_styles_init_chained_transmit_receive_and_snapshot() {
    for style in [0, 1, 2, 3] {
        let (mut nic, mem, irq) = fixture();
        init(&mut nic, &mem, style);
        let packet = packet();
        mem.write_slice(&packet, GuestAddress(0x8000)).unwrap();
        let stride = if style == 0 { 8 } else { 16 };
        desc(&nic, &mem, 0x3000, 0x8000, 0x8200, 40);
        desc(&nic, &mem, 0x3000 + stride, 0x8028, 0x8100, 60);
        desc(&nic, &mem, 0x2000, 0x9000, 0x8000, 64);
        desc(&nic, &mem, 0x2000 + stride, 0xa000, 0x8000, 64);
        let mut frames = Frames::default();
        nic.service(0, &mut frames);
        assert_eq!(frames.tx.pop_front().unwrap(), packet, "style {style}");
        assert!(irq.0.load(Ordering::SeqCst));
        assert_ne!(get(&mut nic, 0) & TINT, 0);
        csr(&mut nic, 0, 0x240);
        assert!(!irq.0.load(Ordering::SeqCst));
        let saved = nic.save_state().unwrap();
        Resettable::reset(&mut nic).unwrap();
        nic.restore_state(&saved).unwrap();
        frames.rx.push_back(packet.clone());
        nic.service(0, &mut frames);
        assert!(irq.0.load(Ordering::SeqCst));
        let first = nic.descriptor(0x2000, 0).unwrap();
        let second = nic.descriptor(0x2000, 1).unwrap();
        assert_eq!(first.flags & 0x8300, 0x200);
        assert_eq!(second.flags & 0x8300, 0x100);
        assert_eq!(second.message & 0xfff, 104);
        let mut data = [0; 104];
        mem.read_slice(&mut data[..64], GuestAddress(0x9000))
            .unwrap();
        mem.read_slice(&mut data[64..], GuestAddress(0xa000))
            .unwrap();
        assert_eq!(&data[..100], &packet);
        assert_eq!(&data[100..], &crc32(&packet).to_le_bytes());
    }
}

#[test]
fn word_dword_alias_prom_soft_and_hard_reset() {
    let (mut nic, _, _) = fixture();
    let mut prom = [0; 16];
    nic.bar_read(0, 0, &mut prom);
    assert_eq!(&prom[..6], &MAC);
    assert_eq!(&prom[14..], &[0x57, 0x57]);
    assert_eq!(
        le16(&prom, 12),
        prom[..12]
            .iter()
            .chain(&prom[14..])
            .map(|v| u16::from(*v))
            .sum::<u16>()
    );
    nic.bar_write(0, 0x10, &0u32.to_le_bytes());
    csr(&mut nic, 58, 3);
    assert_eq!(get(&mut nic, 58), 0x303);
    assert_eq!(get(&mut nic, 88), 0x1003);
    let mut reset = [0; 4];
    nic.bar_read(1, 0x18, &mut reset);
    assert!(!nic.state.dword);
    assert_eq!(get(&mut nic, 58), 0x303);
    Resettable::reset(&mut nic).unwrap();
    assert_eq!(get(&mut nic, 58), 0x200);
    assert_eq!(get(&mut nic, 0), 4);
}

#[test]
fn interrupt_masks_filters_and_bad_dma() {
    let (mut nic, mem, irq) = fixture();
    init(&mut nic, &mem, 2);
    csr(&mut nic, 3, RINT);
    desc(&nic, &mem, 0x2000, 0x9000, 0x8000, 2048);
    let mut frames = Frames::default();
    let mut wrong = packet();
    wrong[0] ^= 2;
    frames.rx.push_back(wrong);
    nic.service(0, &mut frames);
    assert_eq!(nic.state.rx_index, 0);
    frames.rx.push_back(packet());
    nic.service(0, &mut frames);
    assert!(!irq.0.load(Ordering::SeqCst));
    assert_ne!(get(&mut nic, 0) & RINT, 0);
    csr(&mut nic, 3, 0);
    assert!(irq.0.load(Ordering::SeqCst));
    desc(&nic, &mem, 0x2010, u32::MAX - 1, 0x8000, 2048);
    frames.rx.push_back(packet());
    nic.service(0, &mut frames);
    assert_ne!(get(&mut nic, 0) & MERR, 0);
    assert_ne!(nic.descriptor(0x2000, 1).unwrap().flags & 0x8000, 0);
}

#[test]
fn incomplete_transmit_reports_underflow_in_the_last_owned_descriptor() {
    let (mut nic, mem, _) = fixture();
    init(&mut nic, &mem, 2);
    mem.write_slice(&packet(), GuestAddress(0x8000)).unwrap();
    desc(&nic, &mem, 0x3000, 0x8000, 0x8200, 40);
    nic.service(0, &mut Frames::default());
    let first = nic.descriptor(0x3000, 0).unwrap();
    assert_eq!(first.flags & 0xc000, 0x4000);
    assert_eq!(first.message & 0xc000_0000, 0xc000_0000);
    assert_eq!(get(&mut nic, 0) & 0x10, 0);
    assert_ne!(get(&mut nic, 0) & TINT, 0);
}
