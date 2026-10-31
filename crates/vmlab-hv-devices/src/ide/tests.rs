use std::sync::atomic::{AtomicBool, Ordering};

use super::*;

#[derive(Default)]
struct Line(AtomicBool);
impl IrqLine for Line {
    fn set_level(&self, high: bool) {
        self.0.store(high, Ordering::SeqCst);
    }
}

struct Driver {
    ide: Ide,
    memory: GuestMemory,
    lines: [Arc<Line>; 2],
    directory: tempfile::TempDir,
}

impl Driver {
    fn new() -> Self {
        let memory = GuestMemory::from_ranges(&[(GuestAddress(0), 0x80000)]).unwrap();
        let lines: [Arc<Line>; 2] = std::array::from_fn(|_| Arc::new(Line::default()));
        let mut ide = Ide::new("ide", memory.clone(), [lines[0].clone(), lines[1].clone()]);
        ide.config_write(1, 0, &[5]);
        ide.config_write(0x40 / 4, 0, &[0, 0x80, 0, 0x80]);
        Self {
            ide,
            memory,
            lines,
            directory: tempfile::tempdir().unwrap(),
        }
    }

    fn disk(&self, name: &str, data: &[u8], read_only: bool) -> Box<dyn DiskImage> {
        let path = self.directory.path().join(name);
        std::fs::write(&path, data).unwrap();
        vmlab_disk::open(&path, vmlab_disk::Format::Raw, read_only).unwrap()
    }

    fn attach(&mut self, channel: usize, unit: usize, data: &[u8]) {
        self.ide
            .attach_disk(
                channel,
                unit,
                self.disk(&format!("{channel}-{unit}"), data, false),
            )
            .unwrap();
    }

    fn r(&mut self, channel: usize, reg: u64) -> u8 {
        let mut data = [0];
        self.ide.read_taskfile(channel, reg, &mut data);
        data[0]
    }
    fn w(&mut self, channel: usize, reg: u64, value: u8) {
        self.ide.write_taskfile(channel, reg, &[value]);
    }
    fn irq(&self, channel: usize) -> bool {
        self.lines[channel].0.load(Ordering::SeqCst)
    }
    fn bytes(&mut self, channel: usize, len: usize) -> Vec<u8> {
        let mut data = vec![0; len];
        for word in data.chunks_mut(2) {
            self.ide.read_taskfile(channel, 0, word);
        }
        data
    }
    fn put(&mut self, channel: usize, data: &[u8]) {
        for word in data.chunks(2) {
            self.ide.write_taskfile(channel, 0, word);
        }
    }
    fn address(&mut self, channel: usize, unit: usize, lba: u64, sectors: u16, ext: bool) {
        self.w(
            channel,
            6,
            0xe0 | (unit as u8) << 4 | if ext { 0 } else { (lba >> 24) as u8 & 15 },
        );
        if ext {
            for (reg, value) in [
                (2, (sectors >> 8) as u8),
                (3, (lba >> 24) as u8),
                (4, (lba >> 32) as u8),
                (5, (lba >> 40) as u8),
            ] {
                self.w(channel, reg, value);
            }
        }
        for (reg, value) in [
            (2, sectors as u8),
            (3, lba as u8),
            (4, (lba >> 8) as u8),
            (5, (lba >> 16) as u8),
        ] {
            self.w(channel, reg, value);
        }
    }
    fn prd(&self, address: u32, count: u16, end: bool) {
        let mut data = [0; 8];
        data[..4].copy_from_slice(&address.to_le_bytes());
        data[4..6].copy_from_slice(&count.to_le_bytes());
        data[7] = if end { 0x80 } else { 0 };
        self.memory
            .write_slice(&data, GuestAddress(0x1000))
            .unwrap();
    }
    fn packet(&mut self, channel: usize, cdb: [u8; 12], limit: u16, dma: bool) {
        self.w(channel, 1, u8::from(dma));
        self.w(channel, 4, limit as u8);
        self.w(channel, 5, (limit >> 8) as u8);
        self.w(channel, 7, 0xa0);
        assert_eq!(self.r(channel, 2) & 3, 1);
        self.put(channel, &cdb);
    }
}

#[test]
fn pci_identity_masks_and_legacy_decode() {
    let mut d = Driver::new();
    assert_eq!(d.ide.config_read(0), 0x7010_8086);
    assert_eq!(d.ide.config_read(2), 0x0101_8000);
    assert_eq!(d.ide.config_read(0x2c / 4), 0x1100_1af4);
    d.ide.config_write(0, 0, &[0xff; 4]);
    assert_eq!(d.ide.config_read(0), 0x7010_8086);
    d.ide.config_write(1, 0, &[0xff; 4]);
    assert_eq!(d.ide.config_read(1), 0x0280_0107);
    d.ide.config_write(0x40 / 4, 0, &[0xff; 4]);
    assert_eq!(d.ide.config_read(0x40 / 4), 0xf3ff_f3ff);
    d.ide.config_write(0x20 / 4, 0, &[0xff; 4]);
    assert_eq!(d.ide.config_read(0x20 / 4), 0xffff_fff1);
    d.attach(0, 0, &vec![0; 2048]);
    assert_eq!(d.r(0, 7), READY);
    d.ide.config_write(1, 0, &[0]);
    assert_eq!(d.r(0, 7), 0xff);
    d.ide.config_write(1, 0, &[1]);
    d.ide.config_write(0x40 / 4, 1, &[0]);
    assert_eq!(d.r(0, 7), 0xff);
}

#[test]
fn identify_has_parallel_ata_capabilities_and_per_unit_serials() {
    let mut d = Driver::new();
    for channel in 0..2 {
        for unit in 0..2 {
            d.attach(channel, unit, &vec![0; 65536]);
        }
    }
    let mut serials = std::collections::BTreeSet::new();
    for channel in 0..2 {
        for unit in 0..2 {
            d.w(channel, 6, 0xa0 | (unit as u8) << 4);
            d.w(channel, 7, 0xec);
            assert!(d.irq(channel));
            assert_eq!(d.r(channel, 7), READY | DRQ);
            assert!(!d.irq(channel));
            let data = d.bytes(channel, 512);
            assert_eq!(data[150..160], [0; 10]);
            assert_eq!(u16::from_le_bytes(data[94..96].try_into().unwrap()), 0x8010);
            serials.insert(data[20..40].to_vec());
            assert_eq!(d.r(channel, 7), READY);
        }
    }
    assert_eq!(serials.len(), 4);
}

#[test]
fn pio_lba_read_write_and_interrupt_acknowledgement() {
    let mut d = Driver::new();
    let mut original = vec![0; 4096];
    original[512..1024].fill(0xa5);
    d.attach(0, 0, &original);
    d.address(0, 0, 1, 2, false);
    d.w(0, 7, 0x20);
    assert!(d.irq(0));
    let mut alt = [0];
    d.ide.read_control(0, &mut alt);
    assert_eq!(alt[0], READY | DRQ);
    assert!(d.irq(0));
    assert_eq!(d.r(0, 7), READY | DRQ);
    assert!(!d.irq(0));
    assert_eq!(d.bytes(0, 512), vec![0xa5; 512]);
    assert!(d.irq(0));
    assert_eq!(d.r(0, 2), 1);
    assert_eq!(d.r(0, 3), 2);
    d.r(0, 7);
    assert_eq!(d.bytes(0, 512), vec![0; 512]);
    assert!(!d.irq(0));
    assert_eq!(d.r(0, 7), READY);
    d.address(0, 0, 3, 2, false);
    d.w(0, 7, 0x30);
    assert!(!d.irq(0));
    assert_eq!(d.r(0, 7), READY | DRQ);
    d.put(0, &vec![0x3c; 1024]);
    assert!(d.irq(0));
    assert_eq!(d.r(0, 7), READY);
    d.address(0, 0, 3, 2, false);
    d.w(0, 7, 0x20);
    assert_eq!(d.bytes(0, 1024), vec![0x3c; 1024]);
}

#[test]
fn chs_lba48_multiple_and_invalid_range() {
    let mut d = Driver::new();
    d.attach(0, 0, &vec![0; 1024 * 1024]);
    d.w(0, 6, 0xa0);
    d.w(0, 2, 8);
    d.w(0, 7, 0x91);
    d.w(0, 2, 1);
    d.w(0, 3, 2);
    d.w(0, 4, 1);
    d.w(0, 5, 0);
    d.w(0, 7, 0x30);
    d.put(0, &vec![0x77; 512]);
    d.address(0, 0, 9, 1, true);
    d.w(0, 7, 0x24);
    assert_eq!(d.bytes(0, 512), vec![0x77; 512]);
    d.w(0, 2, 4);
    d.w(0, 7, 0xc6);
    d.address(0, 0, 8, 8, true);
    d.w(0, 7, 0x39);
    assert_eq!(d.ide.channels[0].buffer.len(), 2048);
    d.put(0, &vec![0x51; 4096]);
    d.address(0, 0, 8, 8, true);
    d.w(0, 7, 0x29);
    assert_eq!(d.bytes(0, 4096), vec![0x51; 4096]);
    d.address(0, 0, 2047, 2, true);
    d.w(0, 7, 0x24);
    assert_eq!(d.r(0, 7), 0x41);
    assert_eq!(d.r(0, 1), 0x10);
}

#[test]
fn selection_and_srst_preserve_attachments_and_clear_irqs() {
    let mut d = Driver::new();
    d.attach(0, 0, &vec![0x11; 4096]);
    d.attach(0, 1, &vec![0x22; 4096]);
    d.address(0, 1, 0, 1, false);
    d.w(0, 7, 0x20);
    assert_eq!(d.bytes(0, 512), vec![0x22; 512]);
    d.address(0, 0, 0, 1, false);
    d.w(0, 7, 0x20);
    d.ide.write_control(0, &[2]);
    assert!(!d.irq(0));
    d.ide.write_control(0, &[0]);
    assert!(d.irq(0));
    d.ide.write_control(0, &[4]);
    assert_eq!(d.r(0, 7), 0x80);
    assert!(!d.irq(0));
    d.ide.write_control(0, &[0]);
    assert_eq!(d.r(0, 7), READY);
    assert_eq!(d.r(0, 2), 1);
    assert_eq!(d.r(0, 3), 1);
    d.address(0, 0, 0, 1, false);
    d.w(0, 7, 0x20);
    assert_eq!(d.bytes(0, 512), vec![0x11; 512]);
}

#[test]
fn dma_reads_writes_and_start_order() {
    let mut d = Driver::new();
    d.attach(1, 0, &vec![0x64; 65536]);
    d.prd(0x20000, 1024, true);
    d.ide.bar_write(4, 12, &0x1000u32.to_le_bytes());
    d.address(1, 0, 0, 2, false);
    d.w(1, 7, 0xc8);
    assert!(!d.irq(1));
    d.ide.bar_write(4, 8, &[9]);
    assert!(d.irq(1));
    let mut out = vec![0; 1024];
    d.memory
        .read_slice(&mut out, GuestAddress(0x20000))
        .unwrap();
    assert_eq!(out, vec![0x64; 1024]);
    let mut status = [0];
    d.ide.bar_read(4, 10, &mut status);
    assert_eq!(status[0] & 7, 4);
    d.ide.bar_write(4, 10, &[4]);
    d.ide.bar_read(4, 10, &mut status);
    assert_eq!(status[0] & 7, 0);
    d.ide.bar_write(4, 8, &[0]);
    d.memory
        .write_slice(&vec![0x3a; 1024], GuestAddress(0x20000))
        .unwrap();
    d.ide.bar_write(4, 8, &[1]);
    d.address(1, 0, 5, 2, true);
    d.w(1, 7, 0x35);
    assert_eq!(d.r(1, 7), READY);
    d.address(1, 0, 5, 2, false);
    d.w(1, 7, 0x20);
    assert_eq!(d.bytes(1, 1024), vec![0x3a; 1024]);
}

#[test]
fn dma_zero_count_and_multiple_prds() {
    let mut d = Driver::new();
    d.attach(0, 0, &vec![0x2f; 131072]);
    d.prd(0x20000, 0, false);
    let mut end = [0; 8];
    end[..4].copy_from_slice(&0x40000u32.to_le_bytes());
    end[7] = 0x80;
    d.memory.write_slice(&end, GuestAddress(0x1008)).unwrap();
    d.ide.bar_write(4, 4, &0x1000u32.to_le_bytes());
    d.address(0, 0, 0, 256, false);
    d.w(0, 7, 0xc8);
    d.ide.bar_write(4, 0, &[9]);
    assert_eq!(d.r(0, 7), READY);
    for base in [0x20000, 0x40000] {
        let mut out = vec![0; 65536];
        d.memory.read_slice(&mut out, GuestAddress(base)).unwrap();
        assert_eq!(out, vec![0x2f; 65536]);
    }
}

#[test]
fn dma_rejects_bad_descriptors_and_disabled_mastering_without_writes() {
    for (address, count, end, prd_address, master) in [
        (0x2ff00, 512, true, 0x1000, true),
        (0x20001, 512, true, 0x1000, true),
        (0x20000, 511, true, 0x1000, true),
        (0x80000, 512, true, 0x1000, true),
        (0x20000, 256, true, 0x1000, true),
        (0x20000, 256, false, 0xfffc, true),
        (0x20000, 512, true, 0x1000, false),
    ] {
        let mut d = Driver::new();
        d.attach(0, 0, &vec![0x7f; 1024]);
        d.prd(address, count, end);
        if prd_address != 0x1000 {
            let mut bytes = [0; 8];
            d.memory
                .read_slice(&mut bytes, GuestAddress(0x1000))
                .unwrap();
            d.memory
                .write_slice(&bytes, GuestAddress(prd_address))
                .unwrap();
        }
        if !master {
            d.ide.config_write(1, 0, &[1]);
        }
        d.ide.bar_write(4, 4, &(prd_address as u32).to_le_bytes());
        d.address(0, 0, 0, 1, false);
        d.w(0, 7, 0xc8);
        d.ide.bar_write(4, 0, &[9]);
        assert_eq!(d.r(0, 7), 0x41, "bad PRD {address:#x}/{count}");
        let mut status = [0];
        d.ide.bar_read(4, 2, &mut status);
        assert_eq!(status[0] & 7, 6);
        let mut untouched = [0; 8];
        d.memory
            .read_slice(&mut untouched, GuestAddress(0x20000))
            .unwrap();
        assert_eq!(untouched, [0; 8]);
    }
}

#[test]
fn atapi_signatures_byte_count_phases_and_sense() {
    let mut d = Driver::new();
    let iso = d.disk("cd", &vec![0x59; 4096], true);
    d.ide.attach_cdrom(0, 0, Some(iso)).unwrap();
    assert_eq!(d.r(0, 4), 0x14);
    assert_eq!(d.r(0, 5), 0xeb);
    d.packet(0, [0x12, 0, 0, 0, 35, 0, 0, 0, 0, 0, 0, 0], 8, false);
    assert_eq!(d.r(0, 4), 8);
    assert_eq!(d.r(0, 2) & 3, 2);
    let mut inquiry = Vec::new();
    while d.r(0, 7) & DRQ != 0 {
        let length = d.r(0, 4) as usize | (d.r(0, 5) as usize) << 8;
        let phase = d.bytes(0, length.next_multiple_of(2));
        inquiry.extend_from_slice(&phase[..length]);
    }
    assert_eq!(inquiry.len(), 35);
    assert_eq!(&inquiry[8..16], b"QEMU    ");
    assert_eq!(d.r(0, 2) & 3, 3);
    d.packet(0, [0x28, 0, 0, 0, 0, 1, 0, 0, 1, 0, 0, 0], 512, false);
    assert_eq!(d.bytes(0, 2048), vec![0x59; 2048]);
    assert_eq!(d.r(0, 7), READY);
    d.ide.eject_media(0, 0).unwrap();
    d.packet(0, [0; 12], 512, false);
    assert_eq!(d.r(0, 7), 0x41);
    assert_eq!(d.r(0, 1), 0x64);
    d.packet(0, [3, 0, 0, 0, 18, 0, 0, 0, 0, 0, 0, 0], 512, false);
    let sense = d.bytes(0, 18);
    assert_eq!(sense[2], 6);
    assert_eq!(sense[12], 0x28);
    d.packet(0, [0; 12], 512, false);
    assert_eq!(d.r(0, 1), 0x24);
}

#[test]
fn atapi_dma_lock_media_change_and_snapshot_generation() {
    let mut d = Driver::new();
    d.ide
        .attach_cdrom(0, 0, Some(d.disk("cd", &vec![0x68; 4096], true)))
        .unwrap();
    d.packet(0, [0x1e, 0, 0, 0, 1, 0, 0, 0, 0, 0, 0, 0], 512, false);
    assert!(d.ide.eject_media(0, 0).is_err());
    assert!(
        d.ide
            .insert_media(0, 0, d.disk("locked", &vec![0; 2048], true))
            .is_err()
    );
    d.packet(0, [0x1e, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0], 512, false);
    let state = d.ide.save_state().unwrap();
    d.prd(0x20000, 2048, true);
    d.ide.bar_write(4, 4, &0x1000u32.to_le_bytes());
    d.ide.bar_write(4, 0, &[9]);
    d.packet(0, [0x28, 0, 0, 0, 0, 0, 0, 0, 1, 0, 0, 0], 512, true);
    let mut data = vec![0; 2048];
    d.memory
        .read_slice(&mut data, GuestAddress(0x20000))
        .unwrap();
    assert_eq!(data, vec![0x68; 2048]);
    d.ide.restore_state(&state).unwrap();
    d.ide.eject_media(0, 0).unwrap();
    d.ide
        .insert_media(0, 0, d.disk("new", &vec![0x4f; 2048], true))
        .unwrap();
    assert!(d.ide.restore_state(&state).is_err());
    assert!(d.ide.media_state(0, 0).unwrap().present);
}

#[test]
fn snapshots_resume_mid_pio_and_reset_preserves_disks() {
    let mut d = Driver::new();
    d.attach(0, 0, &vec![0x41; 4096]);
    d.address(0, 0, 0, 1, false);
    d.w(0, 7, 0x30);
    d.put(0, &[0x2c; 128]);
    let state = d.ide.save_state().unwrap();
    d.put(0, &vec![0x3d; 384]);
    d.ide.restore_state(&state).unwrap();
    d.put(0, &vec![0x4e; 384]);
    d.address(0, 0, 0, 1, false);
    d.w(0, 7, 0x20);
    let _ = d.bytes(0, 64);
    let state = d.ide.save_state().unwrap();
    assert_eq!(d.bytes(0, 64), vec![0x2c; 64]);
    d.ide.restore_state(&state).unwrap();
    assert_eq!(d.bytes(0, 64), vec![0x2c; 64]);
    assert_eq!(d.bytes(0, 384), vec![0x4e; 384]);
    let idle = d.ide.save_state().unwrap();
    d.ide.restore_state(&idle).unwrap();
    d.ide.reset().unwrap();
    d.ide.config_write(1, 0, &[5]);
    d.ide.config_write(0x40 / 4, 1, &[0x80]);
    d.address(0, 0, 0, 1, false);
    d.w(0, 7, 0x20);
    assert_eq!(d.bytes(0, 128), vec![0x2c; 128]);
    assert!(!d.ide.drives[0][0].is_none());
}

#[test]
fn read_only_and_invalid_snapshots_fail_without_mutating_state() {
    let mut d = Driver::new();
    d.ide
        .attach_disk(0, 0, d.disk("ro", &vec![0x45; 1024], true))
        .unwrap();
    d.address(0, 0, 0, 1, false);
    d.w(0, 7, 0x30);
    assert_eq!(d.r(0, 7), 0x41);
    assert_eq!(d.r(0, 1), 4);
    let state = d.ide.save_state().unwrap();
    let mut invalid: serde_json::Value = serde_json::from_slice(&state).unwrap();
    invalid["channels"][0]["selected"] = serde_json::json!(3);
    assert!(
        d.ide
            .restore_state(&serde_json::to_vec(&invalid).unwrap())
            .is_err()
    );
    assert_eq!(d.ide.save_state().unwrap(), state);
    assert!(d.ide.restore_state(b"invalid").is_err());
}

#[test]
fn bulk_data_port_access_crosses_pio_blocks() {
    let mut d = Driver::new();
    let data: Vec<u8> = (0..2048).map(|i| (i / 128) as u8).collect();
    d.attach(0, 0, &data);
    d.address(0, 0, 0, 4, false);
    d.w(0, 7, 0x20);
    let mut read = vec![0; 2048];
    d.ide.read_taskfile(0, 0, &mut read);
    assert_eq!(read, data);
    d.address(0, 0, 0, 4, false);
    d.w(0, 7, 0x30);
    d.ide.write_taskfile(0, 0, &vec![0x91; 2048]);
    assert_eq!(d.r(0, 7), READY);
    d.address(0, 0, 0, 4, false);
    d.w(0, 7, 0x20);
    assert_eq!(d.bytes(0, 2048), vec![0x91; 2048]);
}

#[test]
fn snapshots_reject_inconsistent_phase_buffers() {
    let mut d = Driver::new();
    d.attach(0, 0, &vec![0; 4096]);
    d.address(0, 0, 0, 1, false);
    d.w(0, 7, 0x20);
    let state = d.ide.save_state().unwrap();
    let mut invalid: serde_json::Value = serde_json::from_slice(&state).unwrap();
    invalid["channels"][0]["transfer"]["remaining"] = serde_json::json!(1);
    assert!(
        d.ide
            .restore_state(&serde_json::to_vec(&invalid).unwrap())
            .is_err()
    );
    assert_eq!(d.ide.save_state().unwrap(), state);
}

#[test]
fn pc_capture_ide_pci_programming_replays_without_lost_bits() {
    let capture =
        include_str!("../../../../docs/hwref/pc-seabios-legacy/guest/lspci-nnnvvvxxxx.txt");
    let function = capture
        .split("\n\n")
        .find(|section| section.starts_with("00:01.1 "))
        .unwrap();
    let mut config = [0; 256];
    let mut total = 0;
    for line in function.lines() {
        let Some((address, values)) = line.split_once(": ") else {
            continue;
        };
        if address.len() != 2 {
            continue;
        }
        let Ok(offset) = usize::from_str_radix(address, 16) else {
            continue;
        };
        let bytes = values
            .split_whitespace()
            .map(|value| u8::from_str_radix(value, 16).unwrap())
            .collect::<Vec<_>>();
        assert_eq!(bytes.len(), 16);
        config[offset..offset + 16].copy_from_slice(&bytes);
        total += 16;
    }
    assert_eq!(total, 256);
    let mut d = Driver::new();
    for (offset, length) in [(4, 2), (0x40, 2), (0x42, 2), (0x44, 1)] {
        d.ide.config_write(
            offset / 4,
            (offset % 4) as u64,
            &config[offset..offset + length],
        );
        let read = d.ide.config_read(offset / 4).to_le_bytes();
        assert_eq!(
            &read[offset % 4..offset % 4 + length],
            &config[offset..offset + length],
            "captured IDE config {offset:#x}"
        );
    }
}
