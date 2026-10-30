//! ICH9 SATA AHCI function for q35 at 00:1f.2.
//!
//! Register and DMA layouts follow Intel AHCI 1.3.1, sections 3-5 and 10.
//! ATA commands follow T13/1410D, and optical commands follow T10 MMC-4.
//! PCI identity and capabilities match the q35 hardware captures. Commands
//! complete synchronously, including tagged NCQ commands. Port multipliers and
//! power management are not advertised. Disk contents belong to the VMM snapshot.

mod ata;
mod atapi;
#[cfg(test)]
mod tests;

use std::sync::Arc;

use vm_memory::{Bytes, GuestAddress, GuestMemory as _, Permissions};
use vmlab_disk::DiskImage;

use crate::{
    BarInfo, BarKind, BusDevice, DeviceError, GuestMemory, IrqLine, MsiMessage, MsiSink, PciDevice,
    PciSnapshot, Snapshot,
};
use ata::{AtaDisk, Drive};
use atapi::Optical;

const PORTS: usize = 6;
const REGS: usize = 0x1000;
const CAP: u32 = (1 << 31) | (1 << 30) | (1 << 20) | (1 << 18) | (31 << 8) | 5;
const DHRS: u32 = 1;
const PSS: u32 = 2;
const DSS: u32 = 4;
const HBFS: u32 = 1 << 29;
const TFES: u32 = 1 << 30;

enum Payload {
    Bytes(Vec<u8>),
    Read { offset: u64, len: usize },
}
impl Payload {
    fn len(&self) -> usize {
        match self {
            Self::Bytes(data) => data.len(),
            Self::Read { len, .. } => *len,
        }
    }
}

fn port_base(port: usize) -> usize {
    0x100 + port * 0x80
}
fn le32(bytes: &[u8]) -> u32 {
    u32::from_le_bytes(bytes[..4].try_into().unwrap())
}
fn le64(bytes: &[u8]) -> u64 {
    u64::from_le_bytes(bytes[..8].try_into().unwrap())
}

/// Guest-visible state of an optical drive's medium and tray.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MediaState {
    pub present: bool,
    pub tray_open: bool,
    pub locked: bool,
}

/// An AHCI PCI function with six independently attachable SATA devices.
pub struct Ahci {
    id: String,
    memory: GuestMemory,
    irq: Arc<dyn IrqLine>,
    msi: Arc<dyn MsiSink>,
    config: [u8; 256],
    regs: [u8; REGS],
    drives: [Option<Drive>; PORTS],
    index: u32,
    msi_pending: u32,
}

impl Ahci {
    /// Register BAR4 as 32 bytes of I/O and BAR5 as 4 KiB of MMIO.
    /// `irq` is PCI INTx pin A, routed by the machine's PCI root.
    pub fn new(
        id: impl Into<String>,
        memory: GuestMemory,
        irq: Arc<dyn IrqLine>,
        msi: Arc<dyn MsiSink>,
    ) -> Self {
        let mut device = Self {
            id: id.into(),
            memory,
            irq,
            msi,
            config: [0; 256],
            regs: [0; REGS],
            drives: std::array::from_fn(|_| None),
            index: 0,
            msi_pending: 0,
        };
        device.config[..4].copy_from_slice(&0x2922_8086u32.to_le_bytes());
        device.config[6] = 0x10;
        device.config[8..12].copy_from_slice(&0x0106_0102u32.to_le_bytes());
        device.config[14] = 0x80;
        device.config[0x20] = 1;
        device.config[0x2c..0x30].copy_from_slice(&0x1100_1af4u32.to_le_bytes());
        device.config[0x34] = 0x80;
        device.config[0x3c] = 10;
        device.config[0x3d] = 1;
        device.config[0x80..0x84].copy_from_slice(&[5, 0xa8, 0x80, 0]);
        device.config[0x90] = 0x40;
        device.config[0x92] = 0x3f;
        device.config[0xa8..0xb0].copy_from_slice(&[0x12, 0, 0x10, 0, 0x48, 0, 0, 0]);
        device.reset();
        device
    }

    /// Attach an ATA disk before the guest starts. A running port is rejected.
    pub fn attach_disk(
        &mut self,
        port: usize,
        image: Box<dyn DiskImage>,
    ) -> Result<(), DeviceError> {
        if image.size() < 512 || !image.size().is_multiple_of(512) || image.size() / 512 >= 1 << 48
        {
            return Err(DeviceError::Other(
                "ATA disk size must be a nonzero multiple of 512 bytes within LBA48".into(),
            ));
        }
        self.attach(
            port,
            Drive::Disk(AtaDisk {
                image,
                write_cache: true,
                transfer_mode: 0x22,
                ncq_error: [0; 24],
            }),
        )
    }

    /// Attach a removable CD-ROM drive, optionally with read-only ISO media.
    pub fn attach_cdrom(
        &mut self,
        port: usize,
        image: Option<Box<dyn DiskImage>>,
    ) -> Result<(), DeviceError> {
        Self::validate_media(image.as_deref())?;
        self.attach(port, Drive::Cdrom(Optical::new(image)))
    }

    fn validate_media(image: Option<&dyn DiskImage>) -> Result<(), DeviceError> {
        if let Some(image) = image
            && (!image.read_only() || image.size() < 2048 || !image.size().is_multiple_of(2048))
        {
            return Err(DeviceError::Other(
                "CD-ROM media must be read-only, nonempty and 2048-byte aligned".into(),
            ));
        }
        Ok(())
    }

    fn attach(&mut self, port: usize, drive: Drive) -> Result<(), DeviceError> {
        if port >= PORTS
            || self.get32(port_base(port) + 0x18) & 0x11 != 0
            || self.drives[port].is_some()
        {
            return Err(DeviceError::Other(
                "AHCI port is invalid, running or occupied".into(),
            ));
        }
        self.drives[port] = Some(drive);
        self.reset_port(port);
        Ok(())
    }

    /// Replace optical media and close the tray. Guest removal locks apply.
    pub fn insert_media(
        &mut self,
        port: usize,
        image: Box<dyn DiskImage>,
    ) -> Result<(), DeviceError> {
        Self::validate_media(Some(image.as_ref()))?;
        let optical = self.optical_mut(port)?;
        if optical.locked {
            return Err(DeviceError::Other("CD-ROM medium is locked".into()));
        }
        optical.image = Some(image);
        optical.tray_open = false;
        optical.generation = optical.generation.wrapping_add(1);
        optical.changed(2);
        Ok(())
    }

    /// Open the tray and remove the host image. Guest removal locks apply.
    pub fn eject_media(&mut self, port: usize) -> Result<Option<Box<dyn DiskImage>>, DeviceError> {
        let optical = self.optical_mut(port)?;
        if optical.locked {
            return Err(DeviceError::Other("CD-ROM medium is locked".into()));
        }
        optical.tray_open = true;
        optical.generation = optical.generation.wrapping_add(1);
        optical.changed(3);
        Ok(optical.image.take())
    }

    fn optical_mut(&mut self, port: usize) -> Result<&mut Optical, DeviceError> {
        match self.drives.get_mut(port) {
            Some(Some(Drive::Cdrom(optical))) => Ok(optical),
            _ => Err(DeviceError::Other("AHCI port is not a CD-ROM".into())),
        }
    }

    pub fn media_state(&self, port: usize) -> Result<MediaState, DeviceError> {
        match self.drives.get(port) {
            Some(Some(Drive::Cdrom(optical))) => Ok(MediaState {
                present: optical.present(),
                tray_open: optical.tray_open,
                locked: optical.locked,
            }),
            _ => Err(DeviceError::Other("AHCI port is not a CD-ROM".into())),
        }
    }

    /// Access an ATA backing image for the VMM's disk snapshot transaction.
    pub fn disk_mut(&mut self, port: usize) -> Option<&mut (dyn DiskImage + 'static)> {
        match self.drives.get_mut(port) {
            Some(Some(Drive::Disk(disk))) => Some(disk.image.as_mut()),
            _ => None,
        }
    }

    fn get32(&self, offset: usize) -> u32 {
        le32(&self.regs[offset..])
    }
    fn put32(&mut self, offset: usize, value: u32) {
        self.regs[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
    }
    fn address(&self, offset: usize) -> u64 {
        le64(&self.regs[offset..])
    }

    fn reset(&mut self) {
        self.regs.fill(0);
        self.put32(0, CAP);
        self.put32(4, 1 << 31);
        self.put32(0xc, 0x3f);
        self.put32(0x10, 0x0001_0300);
        for port in 0..PORTS {
            self.reset_port(port);
        }
        self.index = 0;
        self.msi_pending = 0;
        self.irq.set_level(false);
    }

    fn reset_port(&mut self, port: usize) {
        let base = port_base(port);
        self.regs[base..base + 0x80].fill(0);
        let signature = self.drives[port].as_ref().map(Drive::signature);
        self.put32(base + 0x20, if signature.is_some() { 0x50 } else { 0x7f });
        self.put32(base + 0x24, signature.unwrap_or(0xffff_ffff));
        self.put32(base + 0x28, if signature.is_some() { 0x113 } else { 0 });
        self.put32(base + 0x18, 6);
        if let Some(Drive::Cdrom(optical)) = self.drives[port].as_mut() {
            optical.locked = false;
            optical.sense = [0; 3];
        }
        if let Some(Drive::Disk(disk)) = self.drives[port].as_mut() {
            disk.ncq_error = [0; 24];
        }
    }

    fn update_irq(&mut self) {
        let pending = (0..PORTS).fold(0, |mask, port| {
            let base = port_base(port);
            mask | (u32::from(self.get32(base + 0x10) & self.get32(base + 0x14) != 0) << port)
        });
        // Global IS is sticky until the guest acknowledges it.
        self.put32(8, self.get32(8) | pending);
        let active = self.get32(8) != 0 && self.get32(4) & 2 != 0;
        let enabled = self.config[0x82] & 1 != 0;
        self.config[6] = 0x10 | if active && !enabled { 8 } else { 0 };
        self.irq
            .set_level(active && !enabled && self.config[5] & 4 == 0);
        let pending = if active && enabled { self.get32(8) } else { 0 };
        if pending & !self.msi_pending != 0 {
            self.msi.signal(MsiMessage {
                address: le64(&self.config[0x84..]),
                data: u16::from_le_bytes([self.config[0x8c], self.config[0x8d]]) as u32,
            });
        }
        self.msi_pending = pending;
    }

    fn dma_read(&self, address: u64, bytes: &mut [u8]) -> Result<(), ()> {
        self.memory
            .read_slice(bytes, GuestAddress(address))
            .map_err(|_| ())
    }
    fn dma_write(&self, address: u64, bytes: &[u8]) -> Result<(), ()> {
        self.memory
            .write_slice(bytes, GuestAddress(address))
            .map_err(|_| ())
    }

    fn fis(
        &mut self,
        port: usize,
        status: u8,
        error: u8,
        source: &[u8; 20],
        pio: Option<usize>,
    ) -> Result<(), ()> {
        let base = port_base(port);
        self.put32(base + 0x20, status as u32 | ((error as u32) << 8));
        if self.get32(base + 0x18) & 0x10 == 0 || self.config[4] & 4 == 0 {
            return Ok(());
        }
        let fb = self.address(base + 8);
        let mut fis = *source;
        fis[0] = 0x34;
        fis[1] = 0x40;
        fis[2] = status;
        fis[3] = error;
        fis[14..].fill(0);
        self.dma_write(fb.checked_add(0x40).ok_or(())?, &fis)?;
        if let Some(count) = pio {
            fis[0] = 0x5f;
            fis[1] = 0x60;
            fis[2] = 0x58;
            fis[15] = status;
            fis[16..18].copy_from_slice(&(count.min(65534) as u16).to_le_bytes());
            self.dma_write(fb.checked_add(0x20).ok_or(())?, &fis)?;
        }
        Ok(())
    }

    fn initial_fis(&mut self, port: usize) {
        if self.config[4] & 4 == 0 {
            return;
        }
        let Some(drive) = &self.drives[port] else {
            return;
        };
        let sig = drive.signature().to_le_bytes();
        let mut fis = [0; 20];
        fis[4] = sig[1];
        fis[5] = sig[2];
        fis[6] = sig[3];
        fis[12] = sig[0];
        if self.fis(port, 0x50, 1, &fis, None).is_err() {
            self.port_interrupt(port, HBFS);
        } else {
            self.port_interrupt(port, DHRS);
        }
    }

    fn port_interrupt(&mut self, port: usize, flags: u32) {
        let offset = port_base(port) + 0x10;
        self.put32(offset, self.get32(offset) | flags);
        if flags & (TFES | HBFS) != 0 {
            let cmd = port_base(port) + 0x18;
            self.put32(cmd, self.get32(cmd) & !(1 << 15));
        }
    }

    fn run(&mut self, port: usize) {
        let base = port_base(port);
        if self.config[4] & 4 == 0
            || self.get32(4) & (1 << 31) == 0
            || self.get32(base + 0x18) & 0x8011 != 0x8011
            || self.get32(base + 0x28) & 0xf != 3
        {
            return;
        }
        for slot in 0..32 {
            if self.get32(base + 0x38) & (1 << slot) == 0 {
                continue;
            }
            self.put32(
                base + 0x18,
                (self.get32(base + 0x18) & !0x1f00) | ((slot as u32) << 8),
            );
            let result = self.command(port, slot);
            if result.is_err() {
                self.port_interrupt(port, HBFS);
            }
            // A task-file or host-bus error halts this command list until ST is cleared.
            if self.get32(base + 0x18) & (1 << 15) == 0 {
                break;
            }
            self.put32(base + 0x38, self.get32(base + 0x38) & !(1 << slot));
        }
    }

    fn command(&mut self, port: usize, slot: usize) -> Result<(), ()> {
        let base = port_base(port);
        let header_addr = self
            .address(base)
            .checked_add((slot * 32) as u64)
            .ok_or(())?;
        let mut header = [0; 32];
        self.dma_read(header_addr, &mut header)?;
        if !self.memory.check_range(
            GuestAddress(self.address(base + 8)),
            256,
            Permissions::Write,
        ) {
            return Err(());
        }
        self.dma_write(header_addr.checked_add(4).ok_or(())?, &0u32.to_le_bytes())?;
        let flags = u16::from_le_bytes([header[0], header[1]]);
        if flags & 0xf000 != 0 || flags & 0x1f != 5 {
            return Err(());
        }
        let table = le64(&header[8..]);
        if table & 0x7f != 0 {
            return Err(());
        }
        let mut cfis = [0; 20];
        self.dma_read(table, &mut cfis)?;
        if cfis[0] != 0x27 || cfis[1] & 0xf != 0 {
            return Err(());
        }
        if cfis[1] & 0x80 == 0 {
            if cfis[15] & 4 == 0 {
                self.initial_fis(port);
            } else {
                self.put32(base + 0x20, 0x80);
            }
            return Ok(());
        }
        let count = u16::from_le_bytes([header[2], header[3]]) as usize;
        let mut prds = Vec::with_capacity(count);
        let mut capacity = 0usize;
        for index in 0..count {
            let mut bytes = [0; 16];
            self.dma_read(
                table.checked_add(0x80 + (index * 16) as u64).ok_or(())?,
                &mut bytes,
            )?;
            let address = le64(&bytes);
            let raw = le32(&bytes[12..]);
            let len = (raw as usize & 0x3f_ffff) + 1;
            if address & 1 != 0
                || !len.is_multiple_of(2)
                || !self
                    .memory
                    .check_range(GuestAddress(address), len, Permissions::ReadWrite)
            {
                return Err(());
            }
            capacity = capacity.checked_add(len).ok_or(())?;
            prds.push((address, len, raw & (1 << 31) != 0));
        }
        let write = flags & 0x40 != 0;
        let queued = matches!(cfis[2], 0x60 | 0x61);
        if queued && usize::from(cfis[12] >> 3) != slot {
            return Err(());
        }
        let mut input = Vec::new();
        // ATA write lengths are limited by the sector count, never by guest PRDT capacity.
        if write && matches!(cfis[2], 0x35 | 0xca | 0x30 | 0x34 | 0x61) {
            let (_, length) = Self::ata_range(&cfis);
            if length > capacity {
                return Err(());
            }
            input.resize(length, 0);
            let mut copied = 0;
            for &(address, len, _) in &prds {
                let n = len.min(length - copied);
                self.dma_read(address, &mut input[copied..copied + n])?;
                copied += n;
                if copied == length {
                    break;
                }
            }
        }
        let mut packet = [0; 16];
        if cfis[2] == 0xa0 {
            self.dma_read(table.checked_add(0x40).ok_or(())?, &mut packet)?;
        }
        let response = self.execute(port, &cfis, flags, &packet, &input);
        let (mut status, mut error, data, expected) = match response {
            Ok(data) => {
                let transferred = if write { input.len() } else { data.len() };
                (0x50, 0, data, transferred)
            }
            Err(error) => (0x51, error, Payload::Bytes(Vec::new()), 0),
        };
        if !write && data.len() > capacity {
            return Err(());
        }
        let mut copied = 0;
        let mut ioc = false;
        let mut buffer = vec![0; 64 * 1024];
        for &(address, len, interrupt) in &prds {
            let n = len.min(expected - copied);
            if n == 0 {
                break;
            }
            let mut done = 0;
            while done < n {
                let chunk = (n - done).min(buffer.len());
                if !write {
                    match &data {
                        Payload::Bytes(bytes) => {
                            self.dma_write(address + done as u64, &bytes[copied..copied + chunk])?
                        }
                        Payload::Read { offset, .. } => {
                            if let Err(device_error) = self.read_backing(
                                port,
                                offset + copied as u64,
                                &mut buffer[..chunk],
                            ) {
                                status = 0x51;
                                error = device_error;
                                break;
                            }
                            self.dma_write(address + done as u64, &buffer[..chunk])?;
                        }
                    }
                }
                copied += chunk;
                done += chunk;
            }
            if done == len && interrupt {
                ioc = true;
            }
            if copied == expected || error != 0 {
                break;
            }
        }
        let transferred = copied;
        self.dma_write(
            header_addr.checked_add(4).ok_or(())?,
            &(transferred as u32).to_le_bytes(),
        )?;
        if queued {
            if error != 0
                && let Some(Drive::Disk(disk)) = self.drives[port].as_mut()
            {
                disk.ncq_error[..20].copy_from_slice(&cfis);
                disk.ncq_error[20] = status;
                disk.ncq_error[21] = error;
            }
            self.put32(base + 0x20, u32::from(status) | (u32::from(error) << 8));
            let active = 1u32 << slot;
            self.put32(base + 0x34, self.get32(base + 0x34) & !active);
            if self.get32(base + 0x18) & 0x10 != 0 {
                let mut sdb = [0xa1, 0x40, status, error, 0, 0, 0, 0];
                sdb[4..].copy_from_slice(&active.to_le_bytes());
                self.dma_write(self.address(base + 8).checked_add(0x58).ok_or(())?, &sdb)?;
            }
            self.port_interrupt(
                port,
                8 | if error != 0 { TFES } else { 0 } | if ioc { DSS } else { 0 },
            );
            return Ok(());
        }
        let pio = matches!(cfis[2], 0xec | 0xa1 | 0x20 | 0x24 | 0x2f)
            || (cfis[2] == 0xa0 && cfis[3] & 1 == 0);
        let mut source = cfis;
        if cfis[2] == 0xa0 {
            source[12] = 3; // ATAPI command complete: C/D and I/O.
            source[5] = transferred as u8;
            source[6] = (transferred >> 8) as u8;
        }
        self.fis(
            port,
            status,
            error,
            &source,
            (pio && error == 0).then_some(transferred),
        )?;
        self.port_interrupt(
            port,
            DHRS | if error != 0 { TFES } else { 0 }
                | if pio && error == 0 { PSS } else { 0 }
                | if ioc { DSS } else { 0 },
        );
        Ok(())
    }

    fn read_backing(&mut self, port: usize, offset: u64, bytes: &mut [u8]) -> Result<(), u8> {
        match self.drives[port].as_mut() {
            Some(Drive::Disk(disk)) => disk.image.read_at(offset, bytes).map_err(|_| 0x40),
            Some(Drive::Cdrom(optical)) => {
                if optical
                    .image
                    .as_mut()
                    .ok_or(4)?
                    .read_at(offset, bytes)
                    .is_err()
                {
                    optical.sense = [3, 0x11, 0];
                    Err(0x34)
                } else {
                    Ok(())
                }
            }
            None => Err(4),
        }
    }

    fn ata_range(fis: &[u8; 20]) -> (u64, usize) {
        let queued = matches!(fis[2], 0x60 | 0x61);
        let ext = queued || matches!(fis[2], 0x25 | 0x35 | 0x24 | 0x34);
        let mut lba = fis[4] as u64 | ((fis[5] as u64) << 8) | ((fis[6] as u64) << 16);
        let count = if ext {
            lba |= (fis[8] as u64) << 24 | (fis[9] as u64) << 32 | (fis[10] as u64) << 40;
            let raw = u16::from_le_bytes(if queued {
                [fis[3], fis[11]]
            } else {
                [fis[12], fis[13]]
            }) as usize;
            if raw == 0 { 65536 } else { raw }
        } else {
            lba |= ((fis[7] & 0xf) as u64) << 24;
            if fis[12] == 0 { 256 } else { fis[12] as usize }
        };
        (lba * 512, count * 512)
    }

    fn execute(
        &mut self,
        port: usize,
        fis: &[u8; 20],
        flags: u16,
        packet: &[u8; 16],
        input: &[u8],
    ) -> Result<Payload, u8> {
        let drive = self.drives[port].as_mut().ok_or(4u8)?;
        let write = flags & 0x40 != 0;
        match fis[2] {
            0xec if matches!(drive, Drive::Disk(_)) && !write => {
                Ok(Payload::Bytes(drive.identify(port)))
            }
            0xa1 if matches!(drive, Drive::Cdrom(_)) && !write => {
                Ok(Payload::Bytes(drive.identify(port)))
            }
            0xa0 if flags & 0x20 != 0 && (!write || matches!(packet[0], 0x2a | 0xaa)) => {
                match drive {
                    Drive::Cdrom(optical) => optical.packet(packet),
                    _ => Err(4),
                }
            }
            0x25 | 0xc8 | 0x20 | 0x24 | 0x35 | 0xca | 0x30 | 0x34 | 0x60 | 0x61 => {
                let expected_write = matches!(fis[2], 0x35 | 0xca | 0x30 | 0x34 | 0x61);
                if write != expected_write {
                    return Err(4);
                }
                let Drive::Disk(disk) = drive else {
                    return Err(4);
                };
                let image = &mut disk.image;
                let (offset, len) = Self::ata_range(fis);
                if offset
                    .checked_add(len as u64)
                    .is_none_or(|end| end > image.size())
                {
                    return Err(0x10);
                }
                if write {
                    if image.read_only() || input.len() != len {
                        return Err(4);
                    }
                    image.write_at(offset, input).map_err(|_| 0x40)?;
                    if !disk.write_cache || (fis[2] == 0x61 && fis[7] & 0x80 != 0) {
                        image.flush().map_err(|_| 0x40)?;
                    }
                    Ok(Payload::Bytes(Vec::new()))
                } else {
                    Ok(Payload::Read { offset, len })
                }
            }
            0xe7 | 0xea => match drive {
                Drive::Disk(disk) => {
                    disk.image.flush().map_err(|_| 0x40)?;
                    Ok(Payload::Bytes(Vec::new()))
                }
                _ => Err(4),
            },
            0x2f if !write && fis[4] == 0x10 && fis[5..7] == [0, 0] && fis[12..14] == [1, 0] => {
                let Drive::Disk(disk) = drive else {
                    return Err(4);
                };
                let mut log = vec![0; 512];
                if disk.ncq_error[20] == 0 {
                    log[0] = 0x80;
                } else {
                    log[0] = disk.ncq_error[12] >> 3;
                    log[2..4].copy_from_slice(&disk.ncq_error[20..22]);
                    log[4..11].copy_from_slice(&disk.ncq_error[4..11]);
                    log[12] = disk.ncq_error[3];
                    log[13] = disk.ncq_error[11];
                }
                log[511] =
                    0u8.wrapping_sub(log.iter().fold(0u8, |sum, byte| sum.wrapping_add(*byte)));
                Ok(Payload::Bytes(log))
            }
            0xef => {
                if fis[3] == 3
                    && matches!(fis[12], 0x00 | 0x01 | 0x08..=0x0c | 0x20..=0x22 | 0x40..=0x45)
                {
                    match drive {
                        Drive::Disk(disk) => disk.transfer_mode = fis[12],
                        Drive::Cdrom(optical) => optical.transfer_mode = fis[12],
                    }
                    return Ok(Payload::Bytes(Vec::new()));
                }
                let Drive::Disk(disk) = drive else {
                    return Err(4);
                };
                match fis[3] {
                    2 => disk.write_cache = true,
                    0x82 => {
                        disk.image.flush().map_err(|_| 0x40)?;
                        disk.write_cache = false;
                    }
                    _ => return Err(4),
                }
                Ok(Payload::Bytes(Vec::new()))
            }
            _ => Err(4),
        }
    }

    fn write_reg(&mut self, offset: usize, value: u32, mask: u32) {
        let old = self.get32(offset);
        let merged = (old & !mask) | (value & mask);
        match offset {
            4 => {
                if value & mask & 1 != 0 {
                    self.reset();
                } else {
                    self.put32(4, (merged & 2) | (1 << 31));
                }
            }
            8 => self.put32(8, old & !(value & mask)),
            0x100..=0x3ff => {
                let port = (offset - 0x100) / 0x80;
                let base = port_base(port);
                match offset - base {
                    0 | 4 if self.get32(base + 0x18) & 1 == 0 => self.put32(
                        offset,
                        if offset == base {
                            merged & !0x3ff
                        } else {
                            merged
                        },
                    ),
                    8 | 12 if self.get32(base + 0x18) & 0x10 == 0 => self.put32(
                        offset,
                        if offset == base + 8 {
                            merged & !0xff
                        } else {
                            merged
                        },
                    ),
                    0x10 | 0x30 => self.put32(offset, old & !(value & mask)),
                    0x14 => self.put32(offset, merged & 0xfdc0_00ff),
                    0x18 => {
                        let mut cmd = (merged & 0x0100_0011) | 6;
                        if cmd & 1 != 0 {
                            cmd |= if old & 1 == 0 { 1 << 15 } else { old & 0x9f00 };
                        }
                        if cmd & 0x10 != 0 {
                            cmd |= 1 << 14;
                        }
                        self.put32(offset, cmd);
                        if value & mask & 8 != 0 {
                            self.put32(base + 0x20, self.get32(base + 0x20) & !0x88);
                        }
                        if cmd & 1 == 0 {
                            self.put32(base + 0x38, 0);
                            self.put32(base + 0x34, 0);
                        }
                        if old & 0x10 == 0 && cmd & 0x10 != 0 && self.config[4] & 4 != 0 {
                            self.initial_fis(port);
                        }
                    }
                    0x2c => {
                        self.put32(offset, merged & 0xfff);
                        if merged & 0xf == 1 {
                            self.put32(base + 0x28, u32::from(self.drives[port].is_some()));
                            self.put32(base + 0x38, 0);
                            self.put32(base + 0x34, 0);
                        } else if old & 0xf == 1 && merged & 0xf == 0 {
                            self.put32(
                                base + 0x28,
                                if self.drives[port].is_some() {
                                    0x113
                                } else {
                                    0
                                },
                            );
                            self.put32(
                                base + 0x24,
                                self.drives[port]
                                    .as_ref()
                                    .map_or(0xffff_ffff, Drive::signature),
                            );
                            self.initial_fis(port);
                            self.put32(base + 0x30, self.get32(base + 0x30) | (1 << 16));
                            self.port_interrupt(port, (1 << 22) | (1 << 6));
                        }
                    }
                    0x34 | 0x38 => self.put32(offset, old | (value & mask)),
                    _ => {}
                }
                self.run(port);
            }
            _ => {}
        }
        self.update_irq();
    }
}

impl BusDevice for Ahci {
    fn read(&mut self, offset: u64, data: &mut [u8]) {
        for (i, byte) in data.iter_mut().enumerate() {
            *byte = offset
                .checked_add(i as u64)
                .and_then(|a| self.regs.get(a as usize))
                .copied()
                .unwrap_or(0);
        }
    }
    fn write(&mut self, offset: u64, data: &[u8]) {
        let mut index = 0;
        while index < data.len() {
            let Some(address) = offset
                .checked_add(index as u64)
                .filter(|a| *a < REGS as u64)
            else {
                break;
            };
            let shift = (address as usize & 3) * 8;
            let n = (4 - (address as usize & 3)).min(data.len() - index);
            let mut value = 0;
            let mut mask = 0;
            for j in 0..n {
                value |= (data[index + j] as u32) << (shift + j * 8);
                mask |= 0xff << (shift + j * 8);
            }
            self.write_reg(address as usize & !3, value, mask);
            index += n;
        }
    }
}

impl PciDevice for Ahci {
    fn config_read(&mut self, reg: usize) -> u32 {
        if reg >= 64 {
            return 0;
        }
        let raw = le32(&self.config[reg * 4..]);
        match reg {
            8 => (raw & !0x1f) | 1,
            9 => raw & !0xfff,
            _ => raw,
        }
    }
    fn config_write(&mut self, reg: usize, offset: u64, data: &[u8]) {
        let old_master = self.config[4] & 4;
        let Some(start) = (reg as u64)
            .checked_mul(4)
            .and_then(|a| a.checked_add(offset))
        else {
            return;
        };
        for (i, &byte) in data.iter().enumerate() {
            let Some(address) = start.checked_add(i as u64) else {
                break;
            };
            match address {
                4 => self.config[4] = byte & 7,
                5 => self.config[5] = byte & 5,
                0x20..=0x27 | 0x3c | 0x84..=0x8d => self.config[address as usize] = byte,
                0x82 => self.config[0x82] = 0x80 | (byte & 1),
                0x92 => self.config[0x92] = byte & 0x3f,
                _ => {}
            }
        }
        for port in 0..PORTS {
            if old_master == 0
                && self.config[4] & 4 != 0
                && self.get32(port_base(port) + 0x18) & 0x10 != 0
            {
                self.initial_fis(port);
            }
            self.run(port);
        }
        self.update_irq();
    }
    fn bars(&self) -> Vec<BarInfo> {
        vec![
            BarInfo {
                index: 4,
                size: 32,
                kind: BarKind::Io,
            },
            BarInfo {
                index: 5,
                size: 4096,
                kind: BarKind::Mmio32,
            },
        ]
    }
    fn bar_read(&mut self, bar: u8, offset: u64, data: &mut [u8]) {
        match (bar, offset) {
            (5, _) => self.read(offset, data),
            (4, 0x10) if data.len() == 4 => data.copy_from_slice(&self.index.to_le_bytes()),
            (4, 0x14) => self.read(self.index as u64, data),
            _ => data.fill(0),
        }
    }
    fn bar_write(&mut self, bar: u8, offset: u64, data: &[u8]) {
        match (bar, offset) {
            (5, _) => self.write(offset, data),
            (4, 0x10) if data.len() == 4 => self.index = le32(data) & 0xffc,
            (4, 0x14) => self.write(self.index as u64, data),
            _ => {}
        }
    }
}

impl Snapshot for Ahci {
    fn state_id(&self) -> String {
        self.id.clone()
    }
    fn save_state(&self) -> Result<Vec<u8>, DeviceError> {
        let mut state = b"AHC3".to_vec();
        state.extend(self.config);
        state.extend(self.regs);
        state.extend(self.index.to_le_bytes());
        for drive in &self.drives {
            let mut bytes = [0; 48];
            match drive {
                None => {}
                Some(Drive::Disk(disk)) => {
                    bytes[0] = 1;
                    bytes[1] = u8::from(disk.image.read_only());
                    bytes[2] = u8::from(disk.write_cache);
                    bytes[3] = disk.transfer_mode;
                    bytes[8..16].copy_from_slice(&disk.image.size().to_le_bytes());
                    bytes[24..48].copy_from_slice(&disk.ncq_error);
                }
                Some(Drive::Cdrom(optical)) => {
                    bytes[0] = 2;
                    bytes[1] = u8::from(optical.tray_open);
                    bytes[2] = u8::from(optical.locked);
                    bytes[3] = u8::from(optical.attention);
                    bytes[4] = optical.event;
                    bytes[5..8].copy_from_slice(&optical.sense);
                    bytes[8..16].copy_from_slice(
                        &optical.image.as_ref().map_or(0, |i| i.size()).to_le_bytes(),
                    );
                    bytes[16..24].copy_from_slice(&optical.generation.to_le_bytes());
                    bytes[24] = optical.transfer_mode;
                }
            }
            state.extend(bytes);
        }
        Ok(state)
    }
    fn restore_state(&mut self, state: &[u8]) -> Result<(), DeviceError> {
        let invalid =
            || DeviceError::State("AHCI snapshot version, topology or media mismatch".into());
        if state.len() != 4 + 256 + REGS + 4 + PORTS * 48 || &state[..4] != b"AHC3" {
            return Err(invalid());
        }
        let config: [u8; 256] = state[4..260].try_into().unwrap();
        let regs: [u8; REGS] = state[260..260 + REGS].try_into().unwrap();
        let index = le32(&state[260 + REGS..]);
        if config[..4] != self.config[..4]
            || le32(&regs) != CAP
            || le32(&regs[0xc..]) != 0x3f
            || index >= REGS as u32
            || index & 3 != 0
        {
            return Err(invalid());
        }
        let ports = &state[264 + REGS..];
        for (port, drive) in self.drives.iter().enumerate() {
            let bytes = &ports[port * 48..port * 48 + 48];
            let valid = match drive {
                None => bytes[0] == 0,
                Some(Drive::Disk(disk)) => {
                    bytes[0] == 1
                        && bytes[1] == u8::from(disk.image.read_only())
                        && bytes[2] <= 1
                        && matches!(bytes[3], 0 | 1 | 0x08..=0x0c | 0x20..=0x22 | 0x40..=0x45)
                        && le64(&bytes[8..]) == disk.image.size()
                }
                Some(Drive::Cdrom(optical)) => {
                    bytes[0] == 2
                        && bytes[1..4].iter().all(|b| *b <= 1)
                        && le64(&bytes[8..]) == optical.image.as_ref().map_or(0, |i| i.size())
                        && le64(&bytes[16..]) == optical.generation
                        && matches!(bytes[24], 0 | 1 | 0x08..=0x0c | 0x20..=0x22 | 0x40..=0x45)
                }
            };
            if !valid {
                return Err(invalid());
            }
        }
        self.config = config;
        self.regs = regs;
        self.index = index;
        for (port, drive) in self.drives.iter_mut().enumerate() {
            if let Some(Drive::Disk(disk)) = drive {
                let bytes = &ports[port * 48..port * 48 + 48];
                disk.write_cache = bytes[2] != 0;
                disk.transfer_mode = bytes[3];
                disk.ncq_error.copy_from_slice(&bytes[24..48]);
            }
            if let Some(Drive::Cdrom(optical)) = drive {
                let bytes = &ports[port * 48..port * 48 + 48];
                optical.tray_open = bytes[1] != 0;
                optical.locked = bytes[2] != 0;
                optical.attention = bytes[3] != 0;
                optical.event = bytes[4];
                optical.sense.copy_from_slice(&bytes[5..8]);
                optical.transfer_mode = bytes[24];
            }
        }
        self.msi_pending = 0;
        self.update_irq();
        Ok(())
    }
}

impl PciSnapshot for Ahci {
    fn snapshot_id(&self) -> String {
        self.state_id()
    }
    fn save_function_state(&self) -> Result<Vec<u8>, DeviceError> {
        self.save_state()
    }
    fn save_wrapped_state(&self) -> Result<Vec<u8>, DeviceError> {
        Ok(Vec::new())
    }
    fn restore_function_state(&mut self, state: &[u8]) -> Result<(), DeviceError> {
        self.restore_state(state)
    }
    fn restore_wrapped_state(&mut self, state: &[u8]) -> Result<(), DeviceError> {
        if state.is_empty() {
            Ok(())
        } else {
            Err(DeviceError::State("unexpected AHCI wrapped state".into()))
        }
    }
}
