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
fn fixture() -> (E1000, GuestMemory, Arc<Irq>) {
    let mem = GuestMemory::from_ranges(&[(GuestAddress(0), 0x40000)]).unwrap();
    let irq = Arc::new(Irq::default());
    let mut nic = E1000::new("nic", MAC, mem.clone(), irq.clone());
    nic.config_write(1, 0, &7u16.to_le_bytes());
    nic.set_link(true);
    nic.read_reg(ICR);
    (nic, mem, irq)
}
fn write(nic: &mut E1000, reg: usize, value: u32) {
    nic.bar_write(0, reg as u64, &value.to_le_bytes());
}
fn value(nic: &mut E1000, reg: usize) -> u32 {
    let mut bytes = [0; 4];
    nic.bar_read(0, reg as u64, &mut bytes);
    le32(&bytes, 0)
}
fn frame() -> Vec<u8> {
    let mut packet = vec![0x55; 80];
    packet[..6].copy_from_slice(&MAC);
    packet[12..14].copy_from_slice(&[8, 6]);
    packet
}
fn descriptor(mem: &GuestMemory, address: u64, buffer: u64, length: u16, command: u8) {
    let mut d = [0; 16];
    d[..8].copy_from_slice(&buffer.to_le_bytes());
    d[8..10].copy_from_slice(&length.to_le_bytes());
    d[11] = command;
    mem.write_slice(&d, GuestAddress(address)).unwrap();
}
fn tx_ring(nic: &mut E1000) {
    for (reg, val) in [(TDBAL, 0x1000), (TDLEN, 128), (TCTL, 0xa)] {
        write(nic, reg, val);
    }
}
fn rx_ring(nic: &mut E1000, mem: &GuestMemory) {
    for index in 0..8 {
        descriptor(mem, 0x2000 + index * 16, 0x10000 + index * 4096, 0, 0);
    }
    for (reg, val) in [
        (RDBAL, 0x2000),
        (RDLEN, 128),
        (RDT, 7),
        (RCTL, 2 | (1 << 15) | (1 << 26)),
    ] {
        write(nic, reg, val);
    }
}

#[test]
fn eeprom_serial_and_eerd_mac_and_checksum() {
    let (mut nic, _, _) = fixture();
    let mut sum = 0u16;
    for address in 0..64 {
        write(&mut nic, EERD, (address << 8) | 1);
        let data = value(&mut nic, EERD);
        assert_ne!(data & 0x10, 0);
        let word = (data >> 16) as u16;
        sum = sum.wrapping_add(word);
        if address < 3 {
            assert_eq!(word, le16(&MAC, address as usize * 2));
        }
        write(&mut nic, EECD, 0);
        write(&mut nic, EECD, 2);
        let command = 0x180 | address;
        for bit in (0..9).rev() {
            let pins = 2 | ((command >> bit) & 1) << 2;
            write(&mut nic, EECD, pins);
            write(&mut nic, EECD, pins | 1);
        }
        let mut serial = 0;
        for _ in 0..16 {
            write(&mut nic, EECD, 2);
            write(&mut nic, EECD, 3);
            serial = (serial << 1) | u16::from(value(&mut nic, EECD) & 8 != 0);
        }
        assert_eq!(serial, word, "EEPROM word {address}");
    }
    assert_eq!(sum, 0xbaba);
}

#[test]
fn phy_mdic_io_alias_and_resets() {
    let (mut nic, _, irq) = fixture();
    write(&mut nic, MDIC, 1 << 27 | 1 << 21 | 2 << 16);
    assert_eq!(value(&mut nic, MDIC) & 0x5000_ffff, 0x1000_0141);
    write(&mut nic, MDIC, 1 << 27 | 2 << 21);
    assert_ne!(value(&mut nic, MDIC) & (1 << 30), 0);
    nic.bar_write(1, 0, &IMS.to_le_bytes()[..4]);
    nic.bar_write(1, 4, &LSC.to_le_bytes());
    nic.set_link(false);
    assert!(irq.0.load(Ordering::SeqCst));
    write(&mut nic, CTRL, 1 << 26);
    assert!(!irq.0.load(Ordering::SeqCst));
    assert_eq!(value(&mut nic, IMS), 0);
    assert!(nic.state.config.dma());
    Resettable::reset(&mut nic).unwrap();
    assert!(!nic.state.config.dma());
}

#[test]
fn transmit_legacy_chain_wrap_and_receive_scatter() {
    let (mut nic, mem, _) = fixture();
    tx_ring(&mut nic);
    let packet = frame();
    mem.write_slice(&packet, GuestAddress(0x8000)).unwrap();
    descriptor(&mem, 0x1070, 0x8000, 30, 2);
    descriptor(&mem, 0x1000, 0x801e, 50, 0xb);
    write(&mut nic, TDH, 7);
    write(&mut nic, TDT, 1);
    let mut frames = Frames::default();
    nic.service(0, &mut frames);
    assert_eq!(frames.tx.pop_front().unwrap(), packet);
    assert_eq!(value(&mut nic, TDH), 1);
    assert_eq!(read::<1>(&mem, 0x100c), Some([1]));
    rx_ring(&mut nic, &mem);
    write(&mut nic, RCTL, 2 | 3 << 16 | 1 << 26);
    let mut large = packet.clone();
    large.resize(600, 0x76);
    frames.rx.push_back(large.clone());
    nic.service(0, &mut frames);
    assert_eq!(value(&mut nic, RDH), 3);
    let mut received = Vec::new();
    for index in 0..3 {
        let desc = read::<16>(&mem, 0x2000 + index * 16).unwrap();
        assert_eq!(desc[12] & 3, if index == 2 { 3 } else { 1 });
        let mut data = vec![0; usize::from(le16(&desc, 8))];
        mem.read_slice(&mut data, GuestAddress(0x10000 + index * 4096))
            .unwrap();
        received.extend(data);
    }
    assert_eq!(received, large);
}

#[test]
fn interrupt_delay_absolute_limit_masking_and_throttle() {
    let (mut nic, mem, irq) = fixture();
    rx_ring(&mut nic, &mem);
    write(&mut nic, IMS, RXT0);
    write(&mut nic, RDTR, 10);
    write(&mut nic, RADV, 15);
    let mut frames = Frames::default();
    frames.rx.push_back(frame());
    nic.service(0, &mut frames);
    assert!(!irq.0.load(Ordering::SeqCst));
    frames.rx.push_back(frame());
    nic.service(9 * 1024, &mut frames);
    nic.service(6 * 1024, &mut frames);
    assert!(irq.0.load(Ordering::SeqCst));
    assert_ne!(value(&mut nic, ICR) & RXT0, 0);
    assert!(!irq.0.load(Ordering::SeqCst));
    write(&mut nic, ITR, 100);
    write(&mut nic, 0xc8, RXT0);
    assert!(irq.0.load(Ordering::SeqCst));
    value(&mut nic, ICR);
    write(&mut nic, 0xc8, RXT0);
    assert!(!irq.0.load(Ordering::SeqCst));
    nic.service(100 * 256, &mut frames);
    assert!(irq.0.load(Ordering::SeqCst));
    write(&mut nic, 0xd8, RXT0);
    assert!(!irq.0.load(Ordering::SeqCst));
    write(&mut nic, IMS, RXT0);
    assert!(!irq.0.load(Ordering::SeqCst));
    nic.service(100 * 256, &mut frames);
    assert!(irq.0.load(Ordering::SeqCst));
}

#[test]
fn extended_context_checksum_and_snapshot_mid_packet() {
    let (mut nic, mem, _) = fixture();
    tx_ring(&mut nic);
    let mut packet = frame();
    packet[24..26].fill(0);
    mem.write_slice(&packet, GuestAddress(0x8000)).unwrap();
    let mut ctx = [0; 16];
    ctx[0] = 14;
    ctx[1] = 24;
    ctx[2..4].copy_from_slice(&33u16.to_le_bytes());
    ctx[11] = 0x20;
    mem.write_slice(&ctx, GuestAddress(0x1000)).unwrap();
    descriptor(&mem, 0x1010, 0x8000, 40, 0x22);
    mem.write_slice(&[0x10], GuestAddress(0x101a)).unwrap();
    mem.write_slice(&[1], GuestAddress(0x101d)).unwrap();
    write(&mut nic, TDT, 2);
    let mut frames = Frames::default();
    nic.service(0, &mut frames);
    assert!(frames.tx.is_empty());
    let saved = nic.save_state().unwrap();
    nic.state.tx.clear();
    nic.restore_state(&saved).unwrap();
    descriptor(&mem, 0x1020, 0x8028, 40, 0x2b);
    mem.write_slice(&[0x10], GuestAddress(0x102a)).unwrap();
    write(&mut nic, TDT, 3);
    nic.service(0, &mut frames);
    let packet = frames.tx.pop_front().unwrap();
    assert_eq!(checksum(&packet[14..34]), 0);
}

#[test]
fn receive_filters_vlan_crc_and_bad_dma_are_bounded() {
    let (mut nic, mem, _) = fixture();
    rx_ring(&mut nic, &mem);
    let mut wrong = frame();
    wrong[0] ^= 2;
    let mut frames = Frames::default();
    frames.rx.push_back(wrong);
    nic.service(0, &mut frames);
    assert_eq!(value(&mut nic, RDH), 0);
    write(&mut nic, RCTL, 2);
    let packet = frame();
    frames.rx.push_back(packet.clone());
    nic.service(0, &mut frames);
    let desc = read::<16>(&mem, 0x2000).unwrap();
    assert_eq!(le16(&desc, 8), 84);
    assert_eq!(
        read::<4>(&mem, 0x10000 + 80),
        Some(crc32(&packet).to_le_bytes())
    );
    write(&mut nic, RDBAH, u32::MAX);
    frames.rx.push_back(packet);
    nic.service(0, &mut frames);
    assert_ne!(value(&mut nic, ICR) & RXO, 0);
    assert_eq!(value(&mut nic, RDH), 1);
}

#[test]
fn tso_segments_update_ip_tcp_lengths_sequences_checksums_and_flags() {
    let (mut nic, mem, _) = fixture();
    tx_ring(&mut nic);
    let mut packet = vec![0x76; 54 + 2000];
    packet[..6].copy_from_slice(&MAC);
    packet[12..14].copy_from_slice(&[8, 0]);
    packet[14..34].fill(0);
    packet[14] = 0x45;
    packet[18..20].copy_from_slice(&13u16.to_be_bytes());
    packet[22] = 64;
    packet[23] = 6;
    packet[26..34].copy_from_slice(&[10, 0, 0, 1, 10, 0, 0, 2]);
    packet[34..54].fill(0);
    packet[38..42].copy_from_slice(&1234u32.to_be_bytes());
    packet[46] = 0x50;
    packet[47] = 0x19;
    // The stack supplies the unfolded pseudo-header sum without length.
    let mut pseudo = packet[26..34].to_vec();
    pseudo.extend_from_slice(&[0, 6]);
    packet[50..52].copy_from_slice(&(!checksum(&pseudo)).to_be_bytes());
    mem.write_slice(&packet, GuestAddress(0x8000)).unwrap();
    let mut ctx = [0; 16];
    ctx[0] = 14;
    ctx[1] = 24;
    ctx[2..4].copy_from_slice(&33u16.to_le_bytes());
    ctx[4] = 34;
    ctx[5] = 50;
    ctx[11] = 0x27;
    ctx[13] = 54;
    ctx[14..16].copy_from_slice(&1000u16.to_le_bytes());
    mem.write_slice(&ctx, GuestAddress(0x1000)).unwrap();
    descriptor(&mem, 0x1010, 0x8000, packet.len() as u16, 0x2f);
    mem.write_slice(&[0x10], GuestAddress(0x101a)).unwrap();
    mem.write_slice(&[3], GuestAddress(0x101d)).unwrap();
    write(&mut nic, TDT, 2);
    let mut frames = Frames::default();
    nic.service(0, &mut frames);
    assert_eq!(frames.tx.len(), 2);
    for (index, segment) in frames.tx.into_iter().enumerate() {
        assert_eq!(segment.len(), 1054);
        assert_eq!(
            u16::from_be_bytes(segment[16..18].try_into().unwrap()),
            1040
        );
        assert_eq!(
            u16::from_be_bytes(segment[18..20].try_into().unwrap()),
            13 + index as u16
        );
        assert_eq!(
            u32::from_be_bytes(segment[38..42].try_into().unwrap()),
            1234 + index as u32 * 1000
        );
        assert_eq!(segment[47] & 9, if index == 1 { 9 } else { 0 });
        assert_eq!(checksum(&segment[14..34]), 0);
        let mut pseudo = segment[26..34].to_vec();
        pseudo.extend_from_slice(&[0, 6]);
        pseudo.extend_from_slice(&1020u16.to_be_bytes());
        pseudo.extend_from_slice(&segment[34..]);
        assert_eq!(checksum(&pseudo), 0);
    }
}

#[test]
fn receive_ipv4_udp_checksum_vlan_filter_strip_and_tx_delay_restore() {
    let (mut nic, mem, irq) = fixture();
    rx_ring(&mut nic, &mem);
    let mut packet = vec![0; 64];
    packet[..6].copy_from_slice(&MAC);
    packet[12..14].copy_from_slice(&[8, 0]);
    packet[14] = 0x45;
    packet[16..18].copy_from_slice(&50u16.to_be_bytes());
    packet[22] = 64;
    packet[23] = 17;
    packet[26..34].copy_from_slice(&[10, 0, 0, 1, 10, 0, 0, 2]);
    packet[38..40].copy_from_slice(&30u16.to_be_bytes());
    let mut pseudo = packet[26..34].to_vec();
    pseudo.extend_from_slice(&[0, 17, 0, 30]);
    pseudo.extend_from_slice(&packet[34..]);
    packet[40..42].copy_from_slice(&checksum(&pseudo).to_be_bytes());
    let ip_checksum = checksum(&packet[14..34]);
    packet[24..26].copy_from_slice(&ip_checksum.to_be_bytes());
    let expected = packet.clone();
    packet.splice(12..12, [0x81, 0, 0, 37]);
    write(&mut nic, 0x38, 0x8100);
    write(&mut nic, CTRL, 1 << 30);
    write(&mut nic, 0x5000, 0x300);
    write(&mut nic, RCTL, 2 | 1 << 18 | 1 << 26);
    let mut frames = Frames::default();
    frames.rx.push_back(packet.clone());
    nic.service(0, &mut frames);
    assert_eq!(value(&mut nic, RDH), 0);
    write(&mut nic, 0x5604, 1 << 5);
    frames.rx.push_back(packet);
    nic.service(0, &mut frames);
    let desc = read::<16>(&mem, 0x2000).unwrap();
    assert_eq!(desc[12] & 0x6f, 0x6b);
    assert_eq!(desc[13], 0);
    assert_eq!(le16(&desc, 14), 37);
    assert_eq!(read::<64>(&mem, 0x10000).unwrap().as_slice(), expected);
    value(&mut nic, ICR);
    write(&mut nic, IMS, TXDW);
    write(&mut nic, TIDV, 10);
    write(&mut nic, TADV, 20);
    tx_ring(&mut nic);
    mem.write_slice(&expected, GuestAddress(0x8000)).unwrap();
    descriptor(&mem, 0x1000, 0x8000, 64, 0x8b);
    write(&mut nic, TDT, 1);
    nic.service(0, &mut frames);
    assert!(!irq.0.load(Ordering::SeqCst));
    let saved = nic.save_state().unwrap();
    nic.service(100000, &mut frames);
    assert!(irq.0.load(Ordering::SeqCst));
    nic.restore_state(&saved).unwrap();
    assert!(!irq.0.load(Ordering::SeqCst));
    nic.service(10 * 1024 - 1, &mut frames);
    assert!(!irq.0.load(Ordering::SeqCst));
    nic.service(1, &mut frames);
    assert!(irq.0.load(Ordering::SeqCst));
}
