//! P4 guest-interface tests: real device DMA, interrupts and captured PCI bytes.

use super::*;
use crate::virtio::block::Block;
use crate::virtio::console::Console;
use crate::virtio::net::{Net, NetConfig};
use crate::virtio::testing::{MemDisk, RecIrq, RecMsi};
use vm_memory::{Bytes, GuestAddress, Le16, Le32};

const DESC: u64 = 0x10000;
const DATA: u64 = 0x20000;
const HEADER: u64 = 0x21000;
const STATUS: u64 = 0x22000;

#[test]
fn modern_default_queue_size_activates_and_resets() {
    let mut driver = Driver::new();
    for _ in 0..2 {
        driver.pci.bar_write(4, 0x14, &[0]);
        driver.pci.bar_write(4, 0x14, &[3]);
        driver.pci.bar_write(4, 8, &1u32.to_le_bytes());
        driver.pci.bar_write(4, 12, &1u32.to_le_bytes());
        driver.pci.bar_write(4, 0x14, &[11]);
        assert_eq!(read::<2>(&mut driver.pci, 4, 0x18), 128u16.to_le_bytes());
        driver.avail = 0x11000;
        driver.used = 0x12000;
        for (off, addr) in [(0x20, DESC), (0x28, driver.avail), (0x30, driver.used)] {
            driver.pci.bar_write(4, off, &addr.to_le_bytes());
        }
        // SeaBIOS accepts the offered size without writing queue_size.
        driver.pci.bar_write(4, 0x1c, &1u16.to_le_bytes());
        driver.pci.bar_write(4, 0x14, &[15]);
        assert_eq!(read::<1>(&mut driver.pci, 4, 0x14), [15]);
        driver.request(false, 1);
    }
}

fn memory() -> GuestMemory {
    GuestMemory::from_ranges(&[(GuestAddress(0), 1 << 20)]).unwrap()
}

fn block() -> Box<dyn VirtioDevice> {
    Box::new(Block::new("blk", Box::new(MemDisk::new(65536))))
}

fn config_write(pci: &mut VirtioPciDevice, offset: usize, data: &[u8]) {
    pci.config_write(offset / 4, (offset % 4) as u64, data);
}

fn read<const N: usize>(pci: &mut VirtioPciDevice, bar: u8, offset: u64) -> [u8; N] {
    let mut data = [0; N];
    pci.bar_read(bar, offset, &mut data);
    data
}

fn net() -> VirtioPciDevice {
    VirtioPciDevice::new_transitional(
        "net",
        Box::new(
            Net::new(NetConfig {
                id: "net".into(),
                mac: [2, 0, 0, 0, 0, 1],
                mtu: 1500,
                switch: "/nonexistent/vmlab-test-switch".into(),
            })
            .unwrap(),
        ),
        memory(),
        None,
        None,
        4,
    )
    .unwrap()
}

fn rom() -> Vec<u8> {
    let mut image = vec![0; 512];
    image[..3].copy_from_slice(&[0x55, 0xaa, 1]);
    image[0x18] = 0x20;
    image[0x20..0x28].copy_from_slice(b"PCIR\xf4\x1a\x00\x10");
    image[0x30] = 1;
    image[0x35] = 0x80;
    image[511] = 0u8.wrapping_sub(image.iter().fold(0u8, |a, b| a.wrapping_add(*b)));
    image
}

#[test]
fn option_rom_identity_sizing_reads_and_snapshot() {
    let mut absent = net();
    config_write(&mut absent, 0x30, &u32::MAX.to_le_bytes());
    assert_eq!(absent.config_read(12), 0);
    assert!(!absent.bars().iter().any(|bar| bar.index == ROM_BAR));
    let image = rom();
    let mut pci = net().with_option_rom(&image).unwrap();
    assert_eq!(pci.bars().last().unwrap().size, ROM_SIZE as u64);
    config_write(&mut pci, 0x30, &u32::MAX.to_le_bytes());
    assert_eq!(pci.config_read(12), 0xfffc_0000);
    config_write(&mut pci, 0x30, &0xfebf_ffffu32.to_le_bytes());
    assert_eq!(pci.config_read(12), 0xfebc_0001);
    let state = pci.save_state().unwrap();
    config_write(&mut pci, 0x30, &[0]);
    assert_eq!(pci.config_read(12) & 1, 0);
    pci.restore_state(&state).unwrap();
    assert_eq!(pci.config_read(12), 0xfebc_0001);
    assert_eq!(read::<3>(&mut pci, ROM_BAR, 0), [0x55, 0xaa, 1]);
    pci.bar_write(ROM_BAR, 0, &[0; 3]);
    assert_eq!(read::<3>(&mut pci, ROM_BAR, 0), [0x55, 0xaa, 1]);
    assert_eq!(
        read::<4>(&mut pci, ROM_BAR, 511),
        [image[511], 0xff, 0xff, 0xff]
    );
    assert_eq!(read::<4>(&mut pci, ROM_BAR, u64::MAX), [0xff; 4]);
    for bad in [
        vec![],
        vec![0; 512],
        image[..511].to_vec(),
        {
            let mut bad = image.clone();
            bad[0x26] = 1;
            bad
        },
        {
            let mut bad = image.clone();
            bad[100] = 1;
            bad
        },
    ] {
        assert!(net().with_option_rom(&bad).is_err());
    }
    assert!(Driver::new().pci.with_option_rom(&image).is_err());
}

#[test]
fn built_ipxe_rom_headers_and_bytes() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/ipxe");
    if !root.join("virtio-net.rom").exists() {
        eprintln!("SKIP: run just firmware-ipxe-build to test upstream iPXE ROMs");
        return;
    }
    for name in ["1af41000.rom", "1af41000.efirom", "virtio-net.rom"] {
        let image = std::fs::read(root.join(name)).unwrap();
        let mut pci = net().with_option_rom(&image).unwrap();
        let mut bytes = vec![0; image.len()];
        pci.bar_read(ROM_BAR, 0, &mut bytes);
        assert_eq!(bytes, image);
    }
}

struct Driver {
    pci: VirtioPciDevice,
    mem: GuestMemory,
    msi: Arc<RecMsi>,
    irq: Arc<RecIrq>,
    avail: u64,
    used: u64,
}

impl Driver {
    fn new() -> Self {
        let mem = memory();
        let msi = Arc::new(RecMsi::default());
        let irq = Arc::new(RecIrq::default());
        let pci = VirtioPciDevice::new_transitional(
            "pci",
            block(),
            mem.clone(),
            Some(msi.clone()),
            Some(irq.clone()),
            3,
        )
        .unwrap();
        Self {
            pci,
            mem,
            msi,
            irq,
            avail: 0,
            used: 0,
        }
    }

    fn msix(&mut self) {
        config_write(&mut self.pci, 0x9b, &[0x80]);
        for (vector, data) in [(0, 0x41u32), (1, 0x42)] {
            let mut entry = [0u8; 16];
            entry[..8].copy_from_slice(&0xfee0_0000u64.to_le_bytes());
            entry[8..12].copy_from_slice(&data.to_le_bytes());
            self.pci.bar_write(1, vector * 16, &entry);
        }
    }

    fn legacy(&mut self) {
        self.pci.bar_write(0, 18, &[1]);
        self.pci.bar_write(0, 18, &[3]);
        let features = read::<4>(&mut self.pci, 0, 0);
        // Exercise the legacy layout without EVENT_IDX.
        let features = u32::from_le_bytes(features) & !(1 << 29);
        self.pci.bar_write(0, 4, &features.to_le_bytes());
        assert_eq!(read::<4>(&mut self.pci, 0, 4), features.to_le_bytes());
        let size = u16::from_le_bytes(read(&mut self.pci, 0, 12));
        assert_eq!(size, 128);
        self.pci.bar_write(0, 14, &0u16.to_le_bytes());
        self.pci
            .bar_write(0, 8, &((DESC >> 12) as u32).to_le_bytes());
        assert_eq!(
            read::<4>(&mut self.pci, 0, 8),
            ((DESC >> 12) as u32).to_le_bytes()
        );
        self.avail = DESC + 16 * size as u64;
        self.used = (self.avail + 6 + 2 * size as u64).next_multiple_of(4096);
        if self.pci.msix_control() & 0x8000 != 0 {
            self.pci.bar_write(0, 20, &1u16.to_le_bytes());
            self.pci.bar_write(0, 22, &0u16.to_le_bytes());
        }
        // Legacy drivers do not write FEATURES_OK.
        self.pci.bar_write(0, 18, &[7]);
        assert_eq!(read::<1>(&mut self.pci, 0, 18), [7]);
        assert!(self.pci.activated);
        assert_eq!(self.pci.negotiated, Some(features as u64));
    }

    fn modern(&mut self) {
        self.pci.bar_write(4, 0x14, &[3]);
        self.pci.bar_write(4, 8, &1u32.to_le_bytes());
        self.pci.bar_write(4, 12, &1u32.to_le_bytes());
        self.pci.bar_write(4, 0x14, &[11]);
        assert_eq!(read::<1>(&mut self.pci, 4, 0x14), [11]);
        self.pci.bar_write(4, 0x18, &8u16.to_le_bytes());
        self.avail = 0x11000;
        self.used = 0x12000;
        for (off, addr) in [(0x20, DESC), (0x28, self.avail), (0x30, self.used)] {
            self.pci.bar_write(4, off, &addr.to_le_bytes());
        }
        self.pci.bar_write(4, 0x10, &1u16.to_le_bytes());
        self.pci.bar_write(4, 0x1a, &0u16.to_le_bytes());
        self.pci.bar_write(4, 0x1c, &1u16.to_le_bytes());
        self.pci.bar_write(4, 0x14, &[15]);
        assert!(self.pci.activated);
    }

    fn request(&mut self, legacy: bool, index: u16) {
        for (i, (addr, len, flags)) in [(HEADER, 16u32, 1u16), (DATA, 512, 3), (STATUS, 1, 2)]
            .into_iter()
            .enumerate()
        {
            let mut desc = [0; 16];
            desc[..8].copy_from_slice(&addr.to_le_bytes());
            desc[8..12].copy_from_slice(&len.to_le_bytes());
            desc[12..14].copy_from_slice(&flags.to_le_bytes());
            desc[14..16].copy_from_slice(&(i as u16 + 1).to_le_bytes());
            self.mem
                .write_slice(&desc, GuestAddress(DESC + i as u64 * 16))
                .unwrap();
        }
        let mut header = [0u8; 16];
        header[8..].copy_from_slice(&3u64.to_le_bytes());
        self.mem.write_slice(&header, GuestAddress(HEADER)).unwrap();
        self.mem.write_obj(0xffu8, GuestAddress(STATUS)).unwrap();
        self.mem
            .write_obj(
                Le16::from(0),
                GuestAddress(self.avail + 4 + 2 * (index - 1) as u64),
            )
            .unwrap();
        self.mem
            .write_obj(Le16::from(index), GuestAddress(self.avail + 2))
            .unwrap();
        if legacy {
            self.pci.bar_write(0, 16, &0u16.to_le_bytes());
        } else {
            self.pci.bar_write(4, 0x3000, &0u16.to_le_bytes());
        }
        assert_eq!(
            self.mem
                .read_obj::<Le16>(GuestAddress(self.used + 2))
                .unwrap()
                .to_native(),
            index
        );
        assert_eq!(
            self.mem
                .read_obj::<Le32>(GuestAddress(self.used + 8 + 8 * (index - 1) as u64))
                .unwrap()
                .to_native(),
            513
        );
        assert_eq!(self.mem.read_obj::<u8>(GuestAddress(STATUS)).unwrap(), 0);
        let mut data = [0u8; 512];
        self.mem.read_slice(&mut data, GuestAddress(DATA)).unwrap();
        let expected: Vec<u8> = (0..512).map(|i| ((1536 + i) % 251) as u8).collect();
        assert_eq!(data.as_slice(), expected);
    }
}

#[test]
fn transitional_block_legacy_request_intx_and_isr() {
    let mut g = Driver::new();
    g.legacy();
    g.request(true, 1);
    assert!(*g.irq.level.lock());
    assert_ne!(g.pci.config_read(1) & 0x8_0000, 0);
    assert_eq!(read::<1>(&mut g.pci, 0, 19), [1]);
    assert_eq!(read::<1>(&mut g.pci, 0, 19), [0]);
    assert!(!*g.irq.level.lock());
    assert!(g.msi.messages.lock().is_empty());
}

#[test]
fn transitional_block_legacy_msix_vectors_masking_and_config_shift() {
    let mut g = Driver::new();
    let capacity = read::<8>(&mut g.pci, 0, 20);
    assert_eq!(u64::from_le_bytes(capacity), 128);
    g.msix();
    assert_eq!(read::<8>(&mut g.pci, 0, 24), capacity);
    g.legacy();
    assert_eq!(read::<2>(&mut g.pci, 0, 20), 1u16.to_le_bytes());
    assert_eq!(read::<2>(&mut g.pci, 0, 22), 0u16.to_le_bytes());
    g.pci.bar_write(1, 12, &1u32.to_le_bytes());
    g.request(true, 1);
    assert!(g.msi.messages.lock().is_empty());
    assert_eq!(read::<8>(&mut g.pci, 1, 0x800), 1u64.to_le_bytes());
    g.pci.bar_write(1, 12, &[0]);
    assert_eq!(*g.msi.messages.lock(), vec![(0xfee0_0000, 0x41)]);
    assert_eq!(read::<8>(&mut g.pci, 1, 0x800), [0; 8]);
    TransportInterrupt(g.pci.int.clone()).notify_config();
    assert_eq!(
        *g.msi.messages.lock(),
        vec![(0xfee0_0000, 0x41), (0xfee0_0000, 0x42)]
    );
    assert!(!*g.irq.level.lock());
    assert_eq!(read::<1>(&mut g.pci, 0, 19), [3]);
    g.pci.bar_write(0, 22, &3u16.to_le_bytes());
    assert_eq!(read::<2>(&mut g.pci, 0, 22), NO_VECTOR.to_le_bytes());
}

#[test]
fn transitional_block_modern_request_and_snapshot() {
    let mut g = Driver::new();
    g.msix();
    g.modern();
    assert_eq!(read::<8>(&mut g.pci, 4, 0x2000), 128u64.to_le_bytes());
    g.request(false, 1);
    assert_eq!(*g.msi.messages.lock(), vec![(0xfee0_0000, 0x41)]);
    assert_eq!(read::<1>(&mut g.pci, 4, 0x1000), [1]);
    let transport = g.pci.save_state().unwrap();
    let device = g.pci.device().save_state().unwrap();
    g.pci.bar_write(4, 0x14, &[0]);
    g.pci.restore_state(&transport).unwrap();
    g.pci.device_mut().restore_state(&device).unwrap();
    g.request(false, 2);
}

#[test]
fn legacy_snapshot_restores_pfn_features_pci_and_pending_vectors() {
    let mut g = Driver::new();
    g.msix();
    config_write(&mut g.pci, 0x10, &0xc001u32.to_le_bytes());
    config_write(&mut g.pci, 0x14, &0xf000_0000u32.to_le_bytes());
    config_write(&mut g.pci, 0x20, &0xe000_000cu32.to_le_bytes());
    g.legacy();
    g.pci.bar_write(1, 12, &1u32.to_le_bytes());
    g.request(true, 1);
    let transport = g.pci.save_state().unwrap();
    let device = g.pci.device().save_state().unwrap();
    g.pci.bar_write(0, 18, &[0]);
    assert_eq!(read::<4>(&mut g.pci, 0, 8), [0; 4]);
    g.pci.restore_state(&transport).unwrap();
    g.pci.device_mut().restore_state(&device).unwrap();
    assert_eq!(g.pci.save_state().unwrap(), transport);
    assert_eq!(read::<4>(&mut g.pci, 0, 8), 16u32.to_le_bytes());
    assert_eq!(read::<8>(&mut g.pci, 1, 0x800), 1u64.to_le_bytes());
    g.pci.bar_write(1, 12, &0u32.to_le_bytes());
    assert_eq!(*g.msi.messages.lock(), vec![(0xfee0_0000, 0x41)]);
    g.request(true, 2);
    assert_eq!(g.msi.messages.lock().len(), 2);
}

/// Parse lspci's addressed, sixteen-byte hex records within one PCI function.
fn capture(text: &str, id: u16) -> [u8; 256] {
    let mut selected = false;
    let mut bytes = [0; 256];
    let mut records = 0;
    for line in text.lines() {
        if line.starts_with("00:") && line.contains(" Class ") {
            if selected {
                break;
            }
            selected = line.contains(&format!("[1af4:{id:04x}]"));
        }
        if !selected {
            continue;
        }
        let Some((address, payload)) = line.split_once(": ") else {
            continue;
        };
        if address.len() != 2 {
            continue;
        }
        let Ok(address) = usize::from_str_radix(address, 16) else {
            continue;
        };
        let record: Vec<u8> = payload
            .split_whitespace()
            .map(|b| u8::from_str_radix(b, 16).unwrap())
            .collect();
        assert_eq!(record.len(), 16);
        assert_eq!(address, records * 16);
        bytes[address..address + 16].copy_from_slice(&record);
        records += 1;
    }
    assert_eq!(records, 16);
    bytes
}

#[test]
fn config_bytes_match_every_q35_capture() {
    for text in [
        include_str!("../../../../docs/hwref/q35-seabios-linux-generic/guest/lspci-nnnvvvxxxx.txt"),
        include_str!("../../../../docs/hwref/q35-ovmf-win11/guest/lspci-nnnvvvxxxx.txt"),
        include_str!("../../../../docs/hwref/q35-ovmf-win11-secureboot/guest/lspci-nnnvvvxxxx.txt"),
    ] {
        for ty in [1, 2, 3] {
            let id = match ty {
                1 => 0x1000,
                2 => 0x1001,
                _ => 0x1003,
            };
            let expected = capture(text, id);
            let device: Box<dyn VirtioDevice> = match ty {
                1 => Box::new(
                    Net::new(NetConfig {
                        id: "net".into(),
                        mac: [2, 0, 0, 0, 0, 1],
                        mtu: 1500,
                        switch: "/nonexistent/vmlab-test-switch".into(),
                    })
                    .unwrap(),
                ),
                2 => block(),
                _ => Box::new(Console::new("serial", vec![]).unwrap()),
            };
            let vectors =
                (u16::from_le_bytes(expected[0x9a..0x9c].try_into().unwrap()) & 0x7ff) + 1;
            let mut pci = VirtioPciDevice::new_transitional(
                "pci",
                device,
                memory(),
                None,
                Some(Arc::new(RecIrq::default())),
                vectors,
            )
            .unwrap();
            if ty == 1 {
                pci = pci.with_option_rom(&rom()).unwrap();
            }
            // Replay firmware allocation and driver-writable config. All 256
            // resulting bytes, including the full capability chain, must match.
            for (off, len) in [
                (4, 2),
                (0x10, 4),
                (0x14, 4),
                (0x20, 4),
                (0x24, 4),
                (0x30, 4),
                (0x3c, 1),
                (0x9a, 2),
            ] {
                config_write(&mut pci, off, &expected[off..off + len]);
            }
            for off in (0..256).step_by(4) {
                assert_eq!(
                    pci.config_read(off / 4).to_le_bytes(),
                    expected[off..off + 4],
                    "type={ty} offset={off:x}"
                );
            }
        }
    }
}

#[test]
fn bar_probes_pci_indirect_access_and_bounds() {
    let mut g = Driver::new();
    assert_eq!(
        g.pci
            .bars()
            .iter()
            .map(|b| (b.index, b.size))
            .collect::<Vec<_>>(),
        [(0, 128), (1, 4096), (4, 16384)]
    );
    for (off, mask) in [
        (0x10, 0xffff_ff81u32),
        (0x14, 0xffff_f000),
        (0x20, 0xffff_c00c),
        (0x24, 0xffff_ffff),
    ] {
        config_write(&mut g.pci, off, &u32::MAX.to_le_bytes());
        assert_eq!(g.pci.config_read(off / 4), mask);
    }
    // PCI_CFG_CAP indirect access reaches the same BAR register state.
    config_write(&mut g.pci, 0x88, &[4]);
    config_write(&mut g.pci, 0x8c, &0x16u32.to_le_bytes());
    config_write(&mut g.pci, 0x90, &2u32.to_le_bytes());
    config_write(&mut g.pci, 0x94, &99u16.to_le_bytes());
    assert_eq!(g.pci.config_read(0x94 / 4), 99);
    assert_eq!(read::<2>(&mut g.pci, 0, 12), [0; 2]);
    g.pci.bar_write(0, 8, &u32::MAX.to_le_bytes());
    assert_eq!(read::<4>(&mut g.pci, 0, 8), [0; 4]);
    assert_eq!(read::<8>(&mut g.pci, 4, 0x4000), [0; 8]);
    assert_eq!(read::<8>(&mut g.pci, 4, 0x80), [0; 8]);
    g.pci.bar_write(0, 18, &[0]);
    g.pci.bar_write(0, 8, &0u32.to_le_bytes());
    assert_eq!(read::<4>(&mut g.pci, 0, 8), [0; 4]);
    // Reset snapshots must parse even before feature negotiation.
    let state = g.pci.save_state().unwrap();
    g.pci.restore_state(&state).unwrap();
    assert_eq!(g.pci.save_state().unwrap(), state);
    let mut modern = VirtioPciDevice::new("modern", block(), memory(), None, None);
    assert!(modern.restore_state(&state).is_err());
    assert!(g.pci.restore_state(&modern.save_state().unwrap()).is_err());
}
