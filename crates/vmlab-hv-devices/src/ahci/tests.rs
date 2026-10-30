use std::sync::{
    Mutex,
    atomic::{AtomicBool, Ordering},
};

use super::*;

#[derive(Default)]
struct Interrupts {
    level: AtomicBool,
    messages: Mutex<Vec<MsiMessage>>,
}
impl IrqLine for Interrupts {
    fn set_level(&self, high: bool) {
        self.level.store(high, Ordering::SeqCst);
    }
}
impl MsiSink for Interrupts {
    fn signal(&self, message: MsiMessage) {
        self.messages.lock().unwrap().push(message);
    }
}

struct MemoryDisk {
    data: Vec<u8>,
    readonly: bool,
    flushes: Arc<Mutex<usize>>,
}
impl DiskImage for MemoryDisk {
    fn size(&self) -> u64 {
        self.data.len() as u64
    }
    fn read_only(&self) -> bool {
        self.readonly
    }
    fn read_at(&mut self, offset: u64, buf: &mut [u8]) -> vmlab_disk::Result<()> {
        let end = offset as usize + buf.len();
        let bytes = self
            .data
            .get(offset as usize..end)
            .ok_or_else(|| vmlab_disk::Error::Unsupported("test disk range".into()))?;
        buf.copy_from_slice(bytes);
        Ok(())
    }
    fn write_at(&mut self, offset: u64, buf: &[u8]) -> vmlab_disk::Result<()> {
        if self.readonly {
            return Err(vmlab_disk::Error::ReadOnly);
        }
        self.data[offset as usize..offset as usize + buf.len()].copy_from_slice(buf);
        Ok(())
    }
    fn flush(&mut self) -> vmlab_disk::Result<()> {
        *self.flushes.lock().unwrap() += 1;
        Ok(())
    }
    fn discard(&mut self, _: u64, _: u64) -> vmlab_disk::Result<()> {
        Ok(())
    }
    fn snapshots(&self) -> vmlab_disk::Result<Vec<vmlab_disk::SnapshotInfo>> {
        Ok(Vec::new())
    }
    fn snapshot_create(&mut self, _: &str, _: u64, _: u64) -> vmlab_disk::Result<()> {
        Err(vmlab_disk::Error::Unsupported("test disk snapshots".into()))
    }
    fn snapshot_apply(&mut self, _: &str) -> vmlab_disk::Result<vmlab_disk::SnapshotInfo> {
        Err(vmlab_disk::Error::Unsupported("test disk snapshots".into()))
    }
    fn snapshot_delete(&mut self, _: &str) -> vmlab_disk::Result<()> {
        Err(vmlab_disk::Error::Unsupported("test disk snapshots".into()))
    }
    fn write_vmstate(&mut self, _: u64, _: &[u8]) -> vmlab_disk::Result<()> {
        Err(vmlab_disk::Error::Unsupported("test disk vmstate".into()))
    }
    fn read_vmstate(&mut self, _: u64, _: &mut [u8]) -> vmlab_disk::Result<()> {
        Err(vmlab_disk::Error::Unsupported("test disk vmstate".into()))
    }
}
fn image(data: Vec<u8>, readonly: bool) -> Box<dyn DiskImage> {
    Box::new(MemoryDisk {
        data,
        readonly,
        flushes: Arc::default(),
    })
}

struct Driver {
    hba: Ahci,
    memory: GuestMemory,
    irq: Arc<Interrupts>,
}
impl Driver {
    fn new() -> Self {
        let memory = GuestMemory::from_ranges(&[(GuestAddress(0), 0x20000)]).unwrap();
        let irq = Arc::new(Interrupts::default());
        let hba = Ahci::new("ahci-00:1f.2", memory.clone(), irq.clone(), irq.clone());
        Self { hba, memory, irq }
    }
    fn write(&mut self, offset: usize, value: u32) {
        self.hba.bar_write(5, offset as u64, &value.to_le_bytes());
    }
    fn read(&mut self, offset: usize) -> u32 {
        let mut bytes = [0; 4];
        self.hba.bar_read(5, offset as u64, &mut bytes);
        le32(&bytes)
    }
    fn put(&self, address: u64, bytes: &[u8]) {
        self.memory
            .write_slice(bytes, GuestAddress(address))
            .unwrap();
    }
    fn bytes(&self, address: u64, len: usize) -> Vec<u8> {
        let mut bytes = vec![0; len];
        self.memory
            .read_slice(&mut bytes, GuestAddress(address))
            .unwrap();
        bytes
    }
    fn start(&mut self, port: usize) {
        self.hba.config_write(1, 0, &[6]);
        self.write(4, 0x8000_0001);
        assert_eq!(self.read(4), 0x8000_0000);
        self.write(4, 0x8000_0002);
        let base = port_base(port);
        self.write(base, 0x1000);
        self.write(base + 8, 0x2000);
        self.write(base + 0x14, u32::MAX);
        self.write(base + 0x18, 0x10);
        self.write(base + 0x18, 0x11);
        self.ack(port);
    }
    fn ack(&mut self, port: usize) {
        let cmd = self.read(port_base(port) + 0x18);
        let halted = cmd & 0x8001 == 1;
        if halted {
            self.write(port_base(port) + 0x18, cmd & !1);
        }
        self.write(port_base(port) + 0x10, u32::MAX);
        self.write(8, u32::MAX);
        if halted {
            self.write(port_base(port) + 0x18, cmd | 1);
        }
    }
    fn prepare(
        &mut self,
        command: u8,
        flags: u16,
        lba: u64,
        sectors: u16,
        packet: Option<[u8; 16]>,
        prds: &[(u64, usize, bool)],
    ) {
        let mut header = [0; 32];
        header[..2].copy_from_slice(&(5 | flags).to_le_bytes());
        header[2..4].copy_from_slice(&(prds.len() as u16).to_le_bytes());
        header[8..16].copy_from_slice(&0x3000u64.to_le_bytes());
        self.put(0x1000, &header);
        let mut fis = [0; 64];
        fis[0] = 0x27;
        fis[1] = 0x80;
        fis[2] = command;
        fis[3] = u8::from(command == 0xa0);
        fis[4] = lba as u8;
        fis[5] = (lba >> 8) as u8;
        fis[6] = (lba >> 16) as u8;
        fis[7] = 0x40;
        fis[8] = (lba >> 24) as u8;
        fis[9] = (lba >> 32) as u8;
        fis[10] = (lba >> 40) as u8;
        fis[12..14].copy_from_slice(&sectors.to_le_bytes());
        self.put(0x3000, &fis);
        if let Some(cdb) = packet {
            self.put(0x3040, &cdb);
        }
        for (i, &(address, len, ioc)) in prds.iter().enumerate() {
            let mut prd = [0; 16];
            prd[..8].copy_from_slice(&address.to_le_bytes());
            prd[12..16]
                .copy_from_slice(&((len as u32 - 1) | if ioc { 1 << 31 } else { 0 }).to_le_bytes());
            self.put(0x3080 + i as u64 * 16, &prd);
        }
    }
    fn issue(&mut self, port: usize) -> u32 {
        self.write(port_base(port) + 0x38, 1);
        let errors = self.read(port_base(port) + 0x10) & (TFES | HBFS);
        assert_eq!(self.read(port_base(port) + 0x38), u32::from(errors != 0));
        self.read(port_base(port) + 0x20)
    }
    fn command(&mut self, command: u8, lba: u64, sectors: u16, write: bool) -> u32 {
        self.ack(0);
        self.prepare(
            command,
            if write { 0x40 } else { 0 },
            lba,
            sectors,
            None,
            &[(0x4000, 4096, true)],
        );
        self.issue(0)
    }
    fn packet(&mut self, cdb: &[u8]) -> (u32, Vec<u8>) {
        self.ack(0);
        let mut packet = [0; 16];
        packet[..cdb.len()].copy_from_slice(cdb);
        self.prepare(0xa0, 0x20, 0, 0, Some(packet), &[(0x4000, 8192, false)]);
        let status = self.issue(0);
        let len = le32(&self.bytes(0x1004, 4));
        (status, self.bytes(0x4000, len as usize))
    }
    fn cdrom() -> Self {
        let mut driver = Self::new();
        let mut iso = vec![0; 64 * 2048];
        iso[16 * 2048..16 * 2048 + 7].copy_from_slice(b"\x01CD001\x01");
        driver.hba.attach_cdrom(0, Some(image(iso, true))).unwrap();
        driver.start(0);
        driver
    }
}

#[test]
fn ncq_read_write_tags_set_device_bits_and_error_log_survive_snapshot() {
    let mut d = Driver::new();
    d.hba.attach_disk(0, image(vec![0; 4096], false)).unwrap();
    d.start(0);
    assert_ne!(d.read(0) & (1 << 30), 0);
    assert_eq!(d.read(0) & (1 << 24), 0);
    d.put(0x4000, &[0x5a; 512]);
    for (command, flags, tag, lba) in [(0x61, 0x40, 5, 2), (0x60, 0, 13, 2), (0x60, 0, 23, 99)] {
        d.ack(0);
        d.prepare(command, flags, lba, 0, None, &[(0x4000, 512, false)]);
        d.put(0x3003, &[1]);
        d.put(0x300c, &[(tag << 3) as u8]);
        let header = d.bytes(0x1000, 32);
        d.put(0x1000 + tag * 32, &header);
        d.write(port_base(0) + 0x34, 1 << tag);
        d.write(port_base(0) + 0x38, 1 << tag);
        assert_eq!(d.read(port_base(0) + 0x34), 0);
        assert_eq!(d.bytes(0x2058, 1), [0xa1]);
        assert_eq!(le32(&d.bytes(0x205c, 4)), 1 << tag);
        assert_ne!(d.read(port_base(0) + 0x10) & 8, 0);
        if lba == 2 {
            assert_eq!(d.read(port_base(0) + 0x20), 0x50);
            assert_eq!(d.bytes(0x4000, 512), [0x5a; 512]);
        } else {
            assert_ne!(d.read(port_base(0) + 0x10) & TFES, 0);
        }
    }
    let saved = d.hba.save_state().unwrap();
    d.hba.restore_state(&saved).unwrap();
    d.ack(0);
    d.prepare(0x2f, 0, 0x10, 1, None, &[(0x4000, 512, false)]);
    assert_eq!(d.issue(0), 0x50);
    let log = d.bytes(0x4000, 512);
    assert_eq!(log[0], 23);
    assert_eq!(&log[2..5], &[0x51, 0x10, 99]);
    assert_eq!(log.iter().fold(0u8, |sum, byte| sum.wrapping_add(*byte)), 0);
}

#[test]
fn msi_reports_a_second_port_while_the_first_port_is_pending() {
    let mut d = Driver::new();
    d.hba.attach_disk(0, image(vec![0; 4096], false)).unwrap();
    d.hba.attach_cdrom(2, None).unwrap();
    d.start(0);
    d.hba.config_write(32, 2, &[0x81]);
    d.prepare(0xec, 0, 0, 0, None, &[(0x4000, 512, false)]);
    d.issue(0);
    assert_eq!(d.irq.messages.lock().unwrap().len(), 1);
    let base = port_base(2);
    d.write(base + 8, 0x2000);
    d.write(base + 0x14, u32::MAX);
    d.write(base + 0x18, 0x10);
    assert_eq!(d.read(8) & 5, 5);
    assert_eq!(d.irq.messages.lock().unwrap().len(), 2);
}

#[test]
fn pci_identity_capabilities_and_bar_probes() {
    let mut d = Driver::new();
    assert_eq!(d.hba.config_read(0), 0x2922_8086);
    assert_eq!(d.hba.config_read(2), 0x0106_0102);
    assert_eq!(d.hba.config_read(11), 0x1100_1af4);
    assert_eq!(d.hba.config_read(32), 0x0080_a805);
    assert_eq!(d.hba.config_read(42), 0x0010_0012);
    assert_eq!(d.hba.config_read(43), 0x48);
    assert_eq!(d.hba.bars()[0].index, 4);
    assert_eq!(d.hba.bars()[1].size, 4096);
    d.hba.config_write(8, 0, &u32::MAX.to_le_bytes());
    d.hba.config_write(9, 0, &u32::MAX.to_le_bytes());
    assert_eq!(d.hba.config_read(8), 0xffff_ffe1);
    assert_eq!(d.hba.config_read(9), 0xffff_f000);
    d.hba.config_write(0, 0, &[0; 4]);
    assert_eq!(d.hba.config_read(0), 0x2922_8086);
    assert_eq!(d.hba.config_read(usize::MAX), 0);
}

#[test]
fn hba_reset_six_ports_and_indexed_io() {
    let mut d = Driver::new();
    d.hba.attach_disk(5, image(vec![0; 4096], false)).unwrap();
    d.start(5);
    assert_eq!(d.read(0xc), 0x3f);
    assert_eq!(d.read(0x10), 0x10300);
    assert_eq!(d.read(port_base(5) + 0x24), 0x101);
    assert_eq!(d.read(port_base(4) + 0x28), 0);
    assert_eq!(d.read(port_base(5) + 0x18) & 0xc011, 0xc011);
    d.hba.bar_write(4, 0x10, &0xcu32.to_le_bytes());
    let mut data = [0; 4];
    d.hba.bar_read(4, 0x14, &mut data);
    assert_eq!(le32(&data), 0x3f);
    d.write(4, 1);
    assert_eq!(d.read(port_base(5) + 0x18), 6);
    assert_eq!(d.read(4), 1 << 31);
}

#[test]
fn p4_identify_dma_write_read_flush_and_fis() {
    let mut d = Driver::new();
    let flushes = Arc::new(Mutex::new(0));
    d.hba
        .attach_disk(
            0,
            Box::new(MemoryDisk {
                data: vec![0; 128 * 512],
                readonly: false,
                flushes: flushes.clone(),
            }),
        )
        .unwrap();
    d.start(0);
    assert_eq!(d.command(0xec, 0, 0, false), 0x50);
    assert_eq!(le32(&d.bytes(0x1004, 4)), 512);
    let identify = d.bytes(0x4000, 512);
    assert_eq!(le64(&identify[200..]), 128);
    assert_eq!(identify[153] & 1, 1);
    assert_eq!(d.bytes(0x2020, 1), [0x5f]);
    let payload: Vec<u8> = (0..1024).map(|i| (i % 251) as u8).collect();
    d.put(0x4000, &payload);
    assert_eq!(d.command(0x35, 7, 2, true), 0x50);
    d.put(0x4000, &[0; 1024]);
    assert_eq!(d.command(0x25, 7, 2, false), 0x50);
    assert_eq!(d.bytes(0x4000, 1024), payload);
    assert_eq!(d.bytes(0x2040, 4), [0x34, 0x40, 0x50, 0]);
    assert_eq!(le32(&d.bytes(0x1004, 4)), 1024);
    assert_eq!(d.command(0xea, 0, 0, false), 0x50);
    assert_eq!(*flushes.lock().unwrap(), 1);
    assert!(d.irq.level.load(Ordering::SeqCst));
    d.ack(0);
    assert!(!d.irq.level.load(Ordering::SeqCst));
}

#[test]
fn scatter_gather_ioc_and_nonzero_slot() {
    let mut d = Driver::new();
    d.hba
        .attach_disk(0, image(vec![0x5a; 4096], false))
        .unwrap();
    d.start(0);
    d.prepare(
        0x25,
        0,
        0,
        2,
        None,
        &[(0x5000, 256, true), (0x6000, 768, false)],
    );
    let header = d.bytes(0x1000, 32);
    d.put(0x1000 + 31 * 32, &header);
    d.write(0x138, 1 << 31);
    assert_eq!(d.read(0x138), 0);
    assert_eq!(d.bytes(0x5000, 256), vec![0x5a; 256]);
    assert_eq!(d.bytes(0x6000, 768), vec![0x5a; 768]);
    assert_eq!(le32(&d.bytes(0x1004 + 31 * 32, 4)), 1024);
    assert_ne!(d.read(0x110) & DSS, 0);
}

#[test]
fn msi_and_interrupt_acknowledgement() {
    let mut d = Driver::new();
    d.hba.attach_disk(0, image(vec![0; 4096], false)).unwrap();
    d.start(0);
    d.hba.config_write(33, 0, &0xfee0_1000u32.to_le_bytes());
    d.hba.config_write(34, 0, &1u32.to_le_bytes());
    d.hba.config_write(35, 0, &0x42u16.to_le_bytes());
    d.hba.config_write(32, 2, &[0x81]);
    d.command(0xec, 0, 0, false);
    assert_eq!(
        *d.irq.messages.lock().unwrap(),
        [MsiMessage {
            address: 0x1_fee0_1000,
            data: 0x42
        }]
    );
    assert!(!d.irq.level.load(Ordering::SeqCst));
    d.command(0xec, 0, 0, false);
    assert_eq!(d.irq.messages.lock().unwrap().len(), 2);
    d.ack(0);
    d.write(4, 0x8000_0000);
    d.command(0xec, 0, 0, false);
    assert_eq!(d.irq.messages.lock().unwrap().len(), 2);
    d.write(4, 0x8000_0002);
    assert_eq!(d.irq.messages.lock().unwrap().len(), 3);
}

#[test]
fn command_waits_for_bus_master_and_port_start() {
    let mut d = Driver::new();
    d.hba.attach_disk(0, image(vec![0; 4096], false)).unwrap();
    d.start(0);
    d.hba.config_write(1, 0, &[2]);
    d.prepare(0xec, 0, 0, 0, None, &[(0x4000, 512, false)]);
    d.write(0x138, 1);
    assert_eq!(d.read(0x138), 1);
    d.hba.config_write(1, 0, &[6]);
    assert_eq!(d.read(0x138), 0);
    d.write(0x118, 0x10);
    d.write(0x138, 1);
    assert_eq!(d.read(0x138), 1);
    d.write(0x118, 0x11);
    assert_eq!(d.read(0x138), 0);
}

#[test]
fn task_file_errors_for_readonly_range_and_unknown_command() {
    let mut d = Driver::new();
    d.hba.attach_disk(0, image(vec![0; 4096], true)).unwrap();
    d.start(0);
    assert_eq!(d.command(0x35, 0, 1, true), 0x451);
    assert_ne!(d.read(0x110) & TFES, 0);
    assert_eq!(d.command(0x25, 8, 1, false), 0x1051);
    assert_eq!(d.command(0xff, 0, 0, false), 0x451);
    assert_eq!(d.command(0x25, 0, 1, true), 0x451);
}

#[test]
fn malformed_dma_does_not_write_disk_or_panic() {
    let mut d = Driver::new();
    d.hba
        .attach_disk(0, image(vec![0x55; 4096], false))
        .unwrap();
    d.start(0);
    for prds in [
        vec![(0x1ff00, 512, false)],
        vec![(u64::MAX - 1, 512, false)],
        vec![(0x4001, 512, false)],
        vec![(0x4000, 256, false)],
    ] {
        d.ack(0);
        d.prepare(0x35, 0x40, 0, 1, None, &prds);
        d.issue(0);
        assert_ne!(d.read(0x110) & HBFS, 0);
        assert_eq!(le32(&d.bytes(0x1004, 4)), 0);
    }
    assert_eq!(d.command(0x25, 0, 1, false), 0x50);
    assert_eq!(d.bytes(0x4000, 512), vec![0x55; 512]);
    d.hba.write(u64::MAX, &[0xff; 8]);
    let mut bytes = [1; 8];
    d.hba.read(u64::MAX, &mut bytes);
    assert_eq!(bytes, [0; 8]);
}

#[test]
fn p4_atapi_iso_read10_read12_capacity_and_identify() {
    let mut d = Driver::cdrom();
    assert_eq!(d.read(0x124), 0xeb14_0101);
    assert_eq!(d.command(0xa1, 0, 0, false), 0x50);
    assert_eq!(&d.bytes(0x4000, 2), &[0x80, 0x85]);
    let (status, capacity) = d.packet(&[0x25]);
    assert_eq!(status, 0x50);
    assert_eq!(capacity, [0, 0, 0, 63, 0, 0, 8, 0]);
    for cdb in [
        vec![0x28, 0, 0, 0, 0, 16, 0, 0, 1, 0],
        vec![0xa8, 0, 0, 0, 0, 16, 0, 0, 0, 1, 0, 0],
    ] {
        let (status, data) = d.packet(&cdb);
        assert_eq!(status, 0x50);
        assert_eq!(data.len(), 2048);
        assert_eq!(&data[..7], b"\x01CD001\x01");
    }
}

#[test]
fn atapi_inquiry_mode_sense_configuration_and_sense() {
    let mut d = Driver::cdrom();
    let (status, data) = d.packet(&[0x12, 0, 0, 0, 36]);
    assert_eq!(status, 0x50);
    assert_eq!(data[0], 5);
    assert_eq!(data[1], 0x80);
    assert_eq!(&data[8..16], b"VMLAB   ");
    let (_, data) = d.packet(&[0x5a, 0, 0x2a, 0, 0, 0, 0, 0, 64]);
    assert_eq!(data[3], 0x80);
    assert_eq!(data[8], 0x2a);
    let (_, data) = d.packet(&[0x1a, 0, 0x3f, 0, 64]);
    assert_eq!(data[2], 0x80);
    let (_, data) = d.packet(&[0x46, 0, 0, 0, 0, 0, 0, 0, 128]);
    assert_eq!(&data[6..8], &[0, 8]);
    assert_eq!(d.packet(&[0xff]).0, 0x5451);
    let (_, sense) = d.packet(&[3, 0, 0, 0, 18]);
    assert_eq!(sense[2], 5);
    assert_eq!(sense[12], 0x20);
    let (_, sense) = d.packet(&[3, 0, 0, 0, 18]);
    assert_eq!(sense[2], 0);
    assert_eq!(d.packet(&[0x12, 1, 0, 0, 36]).0, 0x5451);
    let (_, data) = d.packet(&[0x12, 0, 0, 0, 4]);
    assert_eq!(data.len(), 4);
}

#[test]
fn p4_eject_insert_unit_attention_and_media_events() {
    let mut d = Driver::cdrom();
    assert_eq!(d.packet(&[0]).0, 0x50);
    d.hba.eject_media(0).unwrap();
    assert_eq!(d.packet(&[0]).0, 0x6451);
    let (_, sense) = d.packet(&[3, 0, 0, 0, 18]);
    assert_eq!(&sense[12..14], &[0x28, 0]);
    assert_eq!(d.packet(&[0]).0, 0x2451);
    let (_, sense) = d.packet(&[3, 0, 0, 0, 18]);
    assert_eq!(&sense[12..14], &[0x3a, 2]);
    let (_, event) = d.packet(&[0x4a, 1, 0, 0, 0x10, 0, 0, 0, 8]);
    assert_eq!(&event[4..6], &[3, 1]);
    d.hba
        .insert_media(0, image(vec![0xa5; 4096], true))
        .unwrap();
    assert_eq!(d.packet(&[0]).0, 0x6451);
    assert_eq!(d.packet(&[0]).0, 0x50);
    let (_, data) = d.packet(&[0x28, 0, 0, 0, 0, 0, 0, 0, 1]);
    assert_eq!(data, vec![0xa5; 2048]);
    let (_, event) = d.packet(&[0x4a, 1, 0, 0, 0x10, 0, 0, 0, 8]);
    assert_eq!(&event[4..6], &[2, 2]);
}

#[test]
fn guest_tray_lock_eject_and_reload() {
    let mut d = Driver::cdrom();
    assert_eq!(
        d.hba.media_state(0).unwrap(),
        MediaState {
            present: true,
            tray_open: false,
            locked: false
        }
    );
    assert_eq!(d.packet(&[0x1e, 0, 0, 0, 1]).0, 0x50);
    assert!(d.hba.eject_media(0).is_err());
    assert!(d.hba.media_state(0).unwrap().locked);
    assert_eq!(d.packet(&[0x1b, 0, 0, 0, 2]).0, 0x5451);
    assert_eq!(d.packet(&[0x1e, 0, 0, 0, 0]).0, 0x50);
    assert_eq!(d.packet(&[0x1b, 0, 0, 0, 2]).0, 0x50);
    assert_eq!(
        d.hba.media_state(0).unwrap(),
        MediaState {
            present: false,
            tray_open: true,
            locked: false
        }
    );
    assert_eq!(d.packet(&[0]).0, 0x6451);
    assert_eq!(d.packet(&[0]).0, 0x2451);
    assert_eq!(d.packet(&[0x1b, 0, 0, 0, 3]).0, 0x50);
    assert_eq!(d.packet(&[0]).0, 0x6451);
    assert_eq!(d.packet(&[0]).0, 0x50);
}

#[test]
fn snapshot_round_trip_and_media_mismatch_are_atomic() {
    let mut d = Driver::cdrom();
    d.packet(&[0x1e, 0, 0, 0, 1]);
    d.packet(&[0xff]);
    let state = d.hba.save_state().unwrap();
    d.packet(&[3, 0, 0, 0, 18]);
    d.packet(&[0x1e, 0, 0, 0, 0]);
    d.write(4, 1);
    d.hba.restore_state(&state).unwrap();
    assert!(d.hba.eject_media(0).is_err());
    let (_, sense) = d.packet(&[3, 0, 0, 0, 18]);
    assert_eq!(sense[2], 5);
    d.packet(&[0x1e, 0, 0, 0, 0]);
    d.hba.insert_media(0, image(vec![0; 4096], true)).unwrap();
    let before = d.hba.save_state().unwrap();
    assert!(d.hba.restore_state(&state).is_err());
    assert_eq!(before, d.hba.save_state().unwrap());
    for len in [0, 3, state.len() - 1] {
        assert!(d.hba.restore_state(&state[..len]).is_err());
    }
}

#[test]
fn comreset_and_software_reset_deliver_signatures() {
    let mut d = Driver::cdrom();
    d.write(0x12c, 1);
    assert_eq!(d.read(0x128), 1);
    d.write(0x12c, 0);
    assert_eq!(d.read(0x128), 0x113);
    assert_eq!(d.read(0x124), 0xeb14_0101);
    d.ack(0);
    d.prepare(0, 0, 0, 0, None, &[]);
    d.put(0x3001, &[0]);
    d.put(0x300f, &[4]);
    assert_eq!(d.issue(0), 0x80);
    d.put(0x300f, &[0]);
    assert_eq!(d.issue(0), 0x150);
    assert_eq!(d.bytes(0x2044, 3), [1, 0x14, 0xeb]);
}

#[test]
fn fatal_errors_retain_slots_until_port_restart() {
    let mut d = Driver::new();
    d.hba.attach_disk(0, image(vec![0; 4096], false)).unwrap();
    d.start(0);
    d.prepare(0xff, 0, 0, 0, None, &[]);
    d.issue(0);
    assert_eq!(d.read(0x138), 1);
    assert_eq!(d.read(0x118) & 0x8001, 1);
    // Acknowledging interrupts cannot restart a halted command engine.
    d.write(0x110, u32::MAX);
    d.write(8, u32::MAX);
    d.prepare(0xec, 0, 0, 0, None, &[(0x4000, 512, false)]);
    d.write(0x138, 1);
    assert_eq!(d.read(0x138), 1);
    assert_eq!(le32(&d.bytes(0x1004, 4)), 0);
    d.ack(0);
    assert_eq!(d.read(0x138), 0);
    assert_eq!(d.issue(0), 0x50);
}

#[test]
fn dma_above_four_gib_and_fis_address_validation() {
    let high = 1u64 << 32;
    let memory =
        GuestMemory::from_ranges(&[(GuestAddress(0), 0x10000), (GuestAddress(high), 0x10000)])
            .unwrap();
    let irq = Arc::new(Interrupts::default());
    let hba = Ahci::new("ahci", memory.clone(), irq.clone(), irq.clone());
    let mut d = Driver { hba, memory, irq };
    d.hba
        .attach_disk(0, image(vec![0xa5; 4096], false))
        .unwrap();
    d.start(0);
    d.write(0x118, 0);
    d.write(0x104, 1);
    d.write(0x10c, 1);
    d.write(0x118, 0x11);
    d.ack(0);
    d.prepare(0x25, 0, 0, 1, None, &[(high + 0x4000, 512, true)]);
    let mut header = d.bytes(0x1000, 32);
    header[8..16].copy_from_slice(&(high + 0x3000).to_le_bytes());
    d.put(high + 0x1000, &header);
    d.put(high + 0x3000, &d.bytes(0x3000, 0x90));
    assert_eq!(d.issue(0), 0x50);
    assert_eq!(d.bytes(high + 0x4000, 512), vec![0xa5; 512]);
    assert_eq!(d.bytes(high + 0x2040, 1), [0x34]);
    assert_eq!(le32(&d.bytes(high + 0x1004, 4)), 512);
    d.write(0x118, 0);
    d.write(0x104, 0);
    d.write(0x10c, 2);
    d.write(0x118, 0x11);
    d.ack(0);
    d.prepare(0x35, 0x40, 0, 1, None, &[(0x4000, 512, false)]);
    d.write(0x138, 1);
    assert_ne!(d.read(0x110) & HBFS, 0);
    let mut disk = [0; 512];
    d.hba.disk_mut(0).unwrap().read_at(0, &mut disk).unwrap();
    assert_eq!(disk, [0xa5; 512]);
}

#[test]
fn set_features_cache_policy_and_transfer_mode_survive_snapshot() {
    let mut d = Driver::new();
    let flushes = Arc::new(Mutex::new(0));
    d.hba
        .attach_disk(
            0,
            Box::new(MemoryDisk {
                data: vec![0; 4096],
                readonly: false,
                flushes: flushes.clone(),
            }),
        )
        .unwrap();
    d.start(0);
    d.prepare(0xef, 0, 0, 0, None, &[]);
    d.put(0x3003, &[0x82]);
    assert_eq!(d.issue(0), 0x50);
    assert_eq!(*flushes.lock().unwrap(), 1);
    d.command(0x35, 0, 1, true);
    assert_eq!(*flushes.lock().unwrap(), 2);
    d.ack(0);
    d.prepare(0xef, 0, 0, 0x45, None, &[]);
    d.put(0x3003, &[3]);
    assert_eq!(d.issue(0), 0x50);
    let saved = d.hba.save_state().unwrap();
    d.command(0xec, 0, 0, false);
    assert_eq!(d.bytes(0x4000 + 170, 1)[0] & 0x20, 0);
    assert_eq!(d.bytes(0x4000 + 177, 1)[0] & 0x20, 0x20);
    d.ack(0);
    d.prepare(0xef, 0, 0, 0, None, &[]);
    d.put(0x3003, &[2]);
    d.issue(0);
    d.hba.restore_state(&saved).unwrap();
    d.command(0xec, 0, 0, false);
    assert_eq!(d.bytes(0x4000 + 170, 1)[0] & 0x20, 0);
}

#[test]
fn atapi_range_zero_count_readonly_and_toc() {
    let mut d = Driver::cdrom();
    assert_eq!(d.packet(&[0x28, 0, 0, 0, 0, 64, 0, 0, 1]).0, 0x5451);
    let (_, sense) = d.packet(&[3, 0, 0, 0, 18]);
    assert_eq!(sense[12], 0x21);
    let (status, data) = d.packet(&[0x28]);
    assert_eq!(status, 0x50);
    assert!(data.is_empty());
    assert_eq!(d.packet(&[0x2a]).0, 0x7451);
    let (_, sense) = d.packet(&[3, 0, 0, 0, 18]);
    assert_eq!(sense[2], 7);
    assert_eq!(sense[12], 0x27);
    let (_, toc) = d.packet(&[0x43, 0, 0, 0, 0, 0, 0, 0, 32]);
    assert_eq!(&toc[..4], &[0, 18, 1, 1]);
    assert_eq!(toc[6], 1);
    assert_eq!(toc[14], 0xaa);
    assert_eq!(u32::from_be_bytes(toc[16..20].try_into().unwrap()), 64);
}

struct TempDir(std::path::PathBuf);
impl TempDir {
    fn new() -> Self {
        let result = std::process::Command::new("mktemp")
            .args(["-d", "--tmpdir", "vmlab-ahci-XXXXXX"])
            .output()
            .unwrap();
        assert!(result.status.success());
        Self(std::path::PathBuf::from(
            String::from_utf8(result.stdout).unwrap().trim(),
        ))
    }
}
impl Drop for TempDir {
    fn drop(&mut self) {
        std::fs::remove_dir_all(&self.0).unwrap();
    }
}

#[test]
fn p4_real_iso_via_raw_disk_image() {
    if std::process::Command::new("xorriso")
        .arg("-version")
        .output()
        .is_err()
    {
        eprintln!("SKIP p4_real_iso_via_raw_disk_image: xorriso is not installed");
        return;
    }
    let temp = TempDir::new();
    let source = temp.0.join("source");
    std::fs::create_dir(&source).unwrap();
    std::fs::write(
        source.join("HELLO.TXT"),
        b"AHCI optical DMA from a real ISO image\n",
    )
    .unwrap();
    let iso_path = temp.0.join("test.iso");
    let output = std::process::Command::new("xorriso")
        .args(["-as", "mkisofs", "-quiet", "-o"])
        .arg(&iso_path)
        .arg(&source)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let expected = std::fs::read(&iso_path).unwrap();
    let mut d = Driver::new();
    d.hba
        .attach_cdrom(
            0,
            Some(vmlab_disk::open(&iso_path, vmlab_disk::Format::Raw, true).unwrap()),
        )
        .unwrap();
    d.start(0);
    let mut actual = Vec::new();
    for lba in (0..expected.len() / 2048).step_by(4) {
        let sectors = (expected.len() / 2048 - lba).min(4) as u16;
        let mut cdb = [0; 12];
        cdb[0] = 0x28;
        cdb[2..6].copy_from_slice(&(lba as u32).to_be_bytes());
        cdb[7..9].copy_from_slice(&sectors.to_be_bytes());
        let (status, data) = d.packet(&cdb);
        assert_eq!(status, 0x50);
        actual.extend(data);
    }
    assert_eq!(actual, expected);
}

#[test]
fn p4_qcow2_disk_and_controller_snapshot_transaction() {
    let temp = TempDir::new();
    let path = temp.0.join("disk.qcow2");
    vmlab_disk::ops::create_qcow2(&path, 2 * 1024 * 1024, None, None).unwrap();
    let mut d = Driver::new();
    d.hba
        .attach_disk(
            0,
            vmlab_disk::open(&path, vmlab_disk::Format::Qcow2, false).unwrap(),
        )
        .unwrap();
    d.start(0);
    d.put(0x4000, &[0x51; 512]);
    assert_eq!(d.command(0x35, 3, 1, true), 0x50);
    let state = d.hba.save_state().unwrap();
    let ram = d.bytes(0, 0x20000);
    let disk = d.hba.disk_mut(0).unwrap();
    disk.write_vmstate(0, &state).unwrap();
    disk.snapshot_create("before", state.len() as u64, 0)
        .unwrap();
    d.put(0x4000, &[0xa2; 512]);
    assert_eq!(d.command(0x35, 3, 1, true), 0x50);
    assert_eq!(d.command(0x25, 3, 1, false), 0x50);
    assert_eq!(d.bytes(0x4000, 512), vec![0xa2; 512]);
    d.write(4, 1);
    let disk = d.hba.disk_mut(0).unwrap();
    disk.snapshot_apply("before").unwrap();
    let mut restored = vec![0; state.len()];
    disk.read_vmstate(0, &mut restored).unwrap();
    d.put(0, &ram);
    d.hba.restore_state(&restored).unwrap();
    assert_eq!(d.command(0x25, 3, 1, false), 0x50);
    assert_eq!(d.bytes(0x4000, 512), vec![0x51; 512]);
    d.hba.disk_mut(0).unwrap().flush().unwrap();
    if let Ok(output) = std::process::Command::new("qemu-img")
        .arg("check")
        .arg(&path)
        .output()
    {
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    } else {
        eprintln!("SKIP qcow2 external check: qemu-img is not installed");
    }
}

#[test]
fn full_size_ata_zero_count_and_large_atapi_read12() {
    let transfer = 33 * 1024 * 1024;
    let memory = GuestMemory::from_ranges(&[(GuestAddress(0), transfer + 0x10000)]).unwrap();
    let irq = Arc::new(Interrupts::default());
    let hba = Ahci::new("ahci", memory.clone(), irq.clone(), irq.clone());
    let mut d = Driver { hba, memory, irq };
    d.hba
        .attach_disk(0, image(vec![0x59; transfer], false))
        .unwrap();
    d.hba
        .attach_cdrom(1, Some(image(vec![0xa6; transfer], true)))
        .unwrap();
    for (port, size, opcode) in [(0, 32 * 1024 * 1024, 0x25), (1, transfer, 0xa0)] {
        d.start(port);
        let prds: Vec<_> = (0..size)
            .step_by(4 * 1024 * 1024)
            .map(|n| (0x10000 + n as u64, (size - n).min(4 * 1024 * 1024), true))
            .collect();
        let mut cdb = [0; 16];
        cdb[0] = 0xa8;
        cdb[6..10].copy_from_slice(&((transfer / 2048) as u32).to_be_bytes());
        d.prepare(
            opcode,
            if port == 1 { 0x20 } else { 0 },
            0,
            0,
            Some(cdb),
            &prds,
        );
        assert_eq!(d.issue(port), 0x50);
        assert_eq!(le32(&d.bytes(0x1004, 4)), size as u32);
        let byte = if port == 0 { 0x59 } else { 0xa6 };
        for offset in (0..size).step_by(4096) {
            assert_eq!(d.bytes(0x10000 + offset as u64, 4096), vec![byte; 4096]);
        }
    }
}

#[test]
fn media_validation_and_ports_are_checked() {
    let mut d = Driver::new();
    assert!(d.hba.attach_disk(6, image(vec![0; 4096], false)).is_err());
    assert!(
        d.hba
            .attach_cdrom(0, Some(image(vec![0; 4096], false)))
            .is_err()
    );
    assert!(
        d.hba
            .attach_cdrom(0, Some(image(vec![0; 2049], true)))
            .is_err()
    );
    d.hba.attach_cdrom(0, None).unwrap();
    d.start(0);
    assert!(d.hba.attach_disk(0, image(vec![0; 4096], false)).is_err());
    assert_eq!(d.packet(&[0]).0, 0x2451);
    assert!(d.hba.insert_media(1, image(vec![0; 4096], true)).is_err());
}
