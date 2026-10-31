//! PIIX3 IDE, using the Intel 82371SB register and PRD formats and ATA/ATAPI-6.
//! Legacy task files use IRQ14/15. BAR4 contains the two bus-master windows.

#[cfg(test)]
mod tests;

use std::sync::Arc;

use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use vm_memory::{Bytes, GuestAddress, GuestMemory as _, Permissions};
use vmlab_disk::DiskImage;

use crate::ata::{AtaDisk, Drive, Payload, Transport, atapi::Optical};
use crate::{
    BarInfo, BarKind, BusDevice, DeviceError, GuestMemory, IrqLine, PciDevice, PciSnapshot,
    Resettable, Snapshot,
};

const READY: u8 = 0x50;
const DRQ: u8 = 8;
const MAX_TRANSFER: usize = 65536 * 512;

#[derive(Clone, Serialize, Deserialize)]
struct Unit {
    regs: [u8; 8],
    hob: [u8; 8],
    status: u8,
    error: u8,
    multiple: u8,
    heads: u8,
    sectors: u8,
    limit: u64,
}

impl Default for Unit {
    fn default() -> Self {
        Self {
            regs: [0; 8],
            hob: [0; 8],
            status: 0,
            error: 1,
            multiple: 0,
            heads: 16,
            sectors: 63,
            limit: 0,
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
enum Phase {
    Idle,
    Read,
    Write,
    Packet,
    DmaRead,
    DmaWrite,
}

#[derive(Clone, Serialize, Deserialize)]
struct Transfer {
    phase: Phase,
    unit: usize,
    offset: u64,
    remaining: usize,
    block: usize,
    packet: bool,
    ext: bool,
    bytes: Option<Vec<u8>>,
}

impl Default for Transfer {
    fn default() -> Self {
        Self {
            phase: Phase::Idle,
            unit: 0,
            offset: 0,
            remaining: 0,
            block: 512,
            packet: false,
            ext: false,
            bytes: None,
        }
    }
}

#[derive(Clone, Default, Serialize, Deserialize)]
struct Channel {
    selected: usize,
    control: u8,
    irq: bool,
    bm_command: u8,
    bm_status: u8,
    prd: u32,
    units: [Unit; 2],
    transfer: Transfer,
    buffer: Vec<u8>,
    position: usize,
    phase_bytes: usize,
    packet_limit: usize,
}

#[derive(Serialize, Deserialize)]
struct Saved {
    version: u8,
    config: Vec<u8>,
    channels: [Channel; 2],
    drives: Vec<Vec<u8>>,
}

/// An Intel 82371SB IDE function, with primary and secondary master/slave units.
pub struct Ide {
    id: String,
    memory: GuestMemory,
    irq: [Arc<dyn IrqLine>; 2],
    config: [u8; 256],
    channels: [Channel; 2],
    drives: [[Option<Drive>; 2]; 2],
}

impl Ide {
    pub fn new(id: impl Into<String>, memory: GuestMemory, irq: [Arc<dyn IrqLine>; 2]) -> Self {
        let mut config = [0; 256];
        config[..4].copy_from_slice(&0x7010_8086u32.to_le_bytes());
        config[6..8].copy_from_slice(&0x0280u16.to_le_bytes());
        config[8..12].copy_from_slice(&0x0101_8000u32.to_le_bytes());
        config[0x20] = 1;
        config[0x2c..0x30].copy_from_slice(&0x1100_1af4u32.to_le_bytes());
        for line in &irq {
            line.set_level(false);
        }
        Self {
            id: id.into(),
            memory,
            irq,
            config,
            channels: Default::default(),
            drives: std::array::from_fn(|_| std::array::from_fn(|_| None)),
        }
    }

    pub fn attach_disk(
        &mut self,
        channel: usize,
        unit: usize,
        image: Box<dyn DiskImage>,
    ) -> Result<(), DeviceError> {
        if image.size() < 512 || !image.size().is_multiple_of(512) || image.size() / 512 >= 1 << 48
        {
            return Err(DeviceError::Other(
                "ATA disk must have a nonzero 512-byte-aligned size within LBA48".into(),
            ));
        }
        self.attach(
            channel,
            unit,
            Drive::Disk(AtaDisk {
                image,
                write_cache: true,
                transfer_mode: 0x22,
                ncq_error: [0; 24],
            }),
        )
    }

    pub fn attach_cdrom(
        &mut self,
        channel: usize,
        unit: usize,
        image: Option<Box<dyn DiskImage>>,
    ) -> Result<(), DeviceError> {
        Optical::validate_media(image.as_deref())?;
        self.attach(channel, unit, Drive::Cdrom(Optical::new(image)))
    }

    fn attach(&mut self, channel: usize, unit: usize, drive: Drive) -> Result<(), DeviceError> {
        if channel >= 2
            || unit >= 2
            || self.drives[channel][unit].is_some()
            || self.channels[channel].transfer.phase != Phase::Idle
        {
            return Err(DeviceError::Other(
                "IDE slot is invalid, occupied or busy".into(),
            ));
        }
        self.drives[channel][unit] = Some(drive);
        self.reset_unit(channel, unit);
        Ok(())
    }

    fn optical(&mut self, channel: usize, unit: usize) -> Result<&mut Optical, DeviceError> {
        match self.drives.get_mut(channel).and_then(|c| c.get_mut(unit)) {
            Some(Some(Drive::Cdrom(optical))) => Ok(optical),
            _ => Err(DeviceError::Other("IDE slot is not a CD-ROM".into())),
        }
    }

    pub fn insert_media(
        &mut self,
        channel: usize,
        unit: usize,
        image: Box<dyn DiskImage>,
    ) -> Result<(), DeviceError> {
        self.optical(channel, unit)?.insert_media(image)
    }

    pub fn eject_media(
        &mut self,
        channel: usize,
        unit: usize,
    ) -> Result<Option<Box<dyn DiskImage>>, DeviceError> {
        self.optical(channel, unit)?.eject_media()
    }

    pub fn media_state(
        &self,
        channel: usize,
        unit: usize,
    ) -> Result<crate::ata::MediaState, DeviceError> {
        match self.drives.get(channel).and_then(|c| c.get(unit)) {
            Some(Some(Drive::Cdrom(optical))) => Ok(optical.media_state()),
            _ => Err(DeviceError::Other("IDE slot is not a CD-ROM".into())),
        }
    }

    fn reset_unit(&mut self, channel: usize, unit: usize) {
        let mut state = Unit::default();
        state.regs[6] = 0xa0 | (unit as u8) << 4;
        if let Some(drive) = &mut self.drives[channel][unit] {
            state.regs[2] = 1;
            state.regs[3] = 1;
            state.status = READY;
            match drive {
                Drive::Disk(disk) => {
                    state.limit = disk.image.size() / 512;
                    disk.transfer_mode = 0x22;
                    disk.write_cache = true;
                }
                Drive::Cdrom(optical) => {
                    state.regs[4] = 0x14;
                    state.regs[5] = 0xeb;
                    optical.locked = false;
                    optical.sense = [0; 3];
                    optical.transfer_mode = 0x22;
                }
            }
        }
        self.channels[channel].units[unit] = state;
    }

    fn enabled(&self, channel: usize) -> bool {
        channel < 2 && self.config[4] & 1 != 0 && self.config[0x41 + channel * 2] & 0x80 != 0
    }

    fn update_irq(&self, channel: usize) {
        let c = &self.channels[channel];
        self.irq[channel].set_level(c.irq && c.control & 6 == 0);
    }

    fn interrupt(&mut self, channel: usize) {
        self.channels[channel].irq = true;
        self.update_irq(channel);
    }

    fn finish(&mut self, channel: usize, error: u8, interrupt: bool) {
        let c = &mut self.channels[channel];
        let unit = c.transfer.unit;
        c.units[unit].status = if error == 0 { READY } else { 0x41 };
        c.units[unit].error = error;
        if c.transfer.packet {
            c.units[unit].regs[2] = 3;
        }
        c.transfer.phase = Phase::Idle;
        c.buffer.clear();
        c.position = 0;
        c.phase_bytes = 0;
        if interrupt {
            self.interrupt(channel);
        }
    }

    /// Read offsets 0..7 of a channel's legacy task file. Data-port widths
    /// consume FIFO bytes; other registers are byte-wide.
    pub fn read_taskfile(&mut self, channel: usize, offset: u64, data: &mut [u8]) {
        if !self.enabled(channel) {
            data.fill(0xff);
            return;
        }
        if offset == 0 {
            self.read_data(channel, data);
            return;
        }
        let c = &mut self.channels[channel];
        let unit = c.selected;
        let value = if self.drives[channel][unit].is_none() {
            0
        } else {
            match offset {
                1 => c.units[unit].error,
                2..=5 if c.control & 0x80 != 0 => c.units[unit].hob[offset as usize],
                2..=6 => c.units[unit].regs[offset as usize],
                7 => c.units[unit].status,
                _ => 0xff,
            }
        };
        data.fill(0xff);
        if let Some(byte) = data.first_mut() {
            *byte = value;
        }
        if offset == 7 {
            c.irq = false;
            self.update_irq(channel);
        }
    }

    pub fn write_taskfile(&mut self, channel: usize, offset: u64, data: &[u8]) {
        if !self.enabled(channel) || data.is_empty() {
            return;
        }
        if offset == 0 {
            self.write_data(channel, data);
            return;
        }
        let value = data[0];
        if self.channels[channel].control & 4 != 0 {
            return;
        }
        match offset {
            1..=5 => {
                let c = &mut self.channels[channel];
                c.control &= !0x80;
                for unit in &mut c.units {
                    unit.hob[offset as usize] = unit.regs[offset as usize];
                    unit.regs[offset as usize] = value;
                }
            }
            6 => {
                let c = &mut self.channels[channel];
                c.selected = usize::from(value >> 4 & 1);
                for (i, unit) in c.units.iter_mut().enumerate() {
                    unit.regs[6] = (value & !0x10) | (i as u8) << 4;
                }
            }
            7 => self.command(channel, value),
            _ => {}
        }
    }

    pub fn read_control(&mut self, channel: usize, data: &mut [u8]) {
        if !self.enabled(channel) {
            data.fill(0xff);
            return;
        }
        let c = &self.channels[channel];
        data.fill(0xff);
        if let Some(byte) = data.first_mut() {
            *byte = if self.drives[channel][c.selected].is_some() {
                c.units[c.selected].status
            } else {
                0
            };
        }
    }

    pub fn write_control(&mut self, channel: usize, data: &[u8]) {
        if !self.enabled(channel) || data.is_empty() {
            return;
        }
        let previous = self.channels[channel].control;
        let value = data[0] & 0x86;
        self.channels[channel].control = value;
        if value & 4 != 0 && previous & 4 == 0 {
            let c = &mut self.channels[channel];
            c.irq = false;
            c.transfer = Transfer::default();
            c.buffer.clear();
            for unit in &mut c.units {
                unit.status = 0x80;
            }
        } else if value & 4 == 0 && previous & 4 != 0 {
            for unit in 0..2 {
                self.reset_unit(channel, unit);
            }
            self.channels[channel].selected = 0;
        }
        self.update_irq(channel);
    }

    fn range(&self, channel: usize, ext: bool) -> Result<(u64, usize), u8> {
        let c = &self.channels[channel];
        let u = &c.units[c.selected];
        let count = if ext {
            let raw = u16::from_le_bytes([u.regs[2], u.hob[2]]);
            if raw == 0 { 65536 } else { raw as usize }
        } else if u.regs[2] == 0 {
            256
        } else {
            u.regs[2] as usize
        };
        let lba = if ext {
            u64::from(u.regs[3])
                | u64::from(u.regs[4]) << 8
                | u64::from(u.regs[5]) << 16
                | u64::from(u.hob[3]) << 24
                | u64::from(u.hob[4]) << 32
                | u64::from(u.hob[5]) << 40
        } else if u.regs[6] & 0x40 != 0 {
            u64::from(u.regs[3])
                | u64::from(u.regs[4]) << 8
                | u64::from(u.regs[5]) << 16
                | u64::from(u.regs[6] & 15) << 24
        } else {
            if u.regs[3] == 0 || u.regs[3] > u.sectors || u.regs[6] & 15 >= u.heads {
                return Err(0x10);
            }
            (u64::from(u16::from_le_bytes([u.regs[4], u.regs[5]])) * u64::from(u.heads)
                + u64::from(u.regs[6] & 15))
                * u64::from(u.sectors)
                + u64::from(u.regs[3] - 1)
        };
        if lba
            .checked_add(count as u64)
            .is_none_or(|end| end > u.limit)
        {
            return Err(0x10);
        }
        Ok((lba * 512, count * 512))
    }

    fn advance_address(&mut self, channel: usize) {
        let c = &mut self.channels[channel];
        let t = &c.transfer;
        if t.packet {
            return;
        }
        let u = &mut c.units[t.unit];
        let lba = t.offset / 512;
        let count = t.remaining / 512;
        u.regs[2] = count as u8;
        if t.ext {
            u.hob[2] = (count >> 8) as u8;
            u.regs[3] = lba as u8;
            u.regs[4] = (lba >> 8) as u8;
            u.regs[5] = (lba >> 16) as u8;
            u.hob[3] = (lba >> 24) as u8;
            u.hob[4] = (lba >> 32) as u8;
            u.hob[5] = (lba >> 40) as u8;
        } else if u.regs[6] & 0x40 != 0 {
            u.regs[3] = lba as u8;
            u.regs[4] = (lba >> 8) as u8;
            u.regs[5] = (lba >> 16) as u8;
            u.regs[6] = (u.regs[6] & 0xf0) | (lba >> 24) as u8 & 15;
        } else {
            u.regs[3] = (lba % u64::from(u.sectors)) as u8 + 1;
            let track = lba / u64::from(u.sectors);
            u.regs[6] = (u.regs[6] & 0xf0) | (track % u64::from(u.heads)) as u8;
            let cylinder = track / u64::from(u.heads);
            u.regs[4] = cylinder as u8;
            u.regs[5] = (cylinder >> 8) as u8;
        }
    }

    fn command(&mut self, channel: usize, opcode: u8) {
        let unit = self.channels[channel].selected;
        if self.drives[channel][unit].is_none() {
            return;
        }
        {
            let c = &mut self.channels[channel];
            c.irq = false;
            c.buffer.clear();
            c.position = 0;
            c.transfer = Transfer {
                unit,
                ..Default::default()
            };
            c.units[unit].status = 0x80;
            c.units[unit].error = 0;
        }
        self.update_irq(channel);
        let error = self.execute(channel, opcode).err().unwrap_or(0);
        if error != 0 {
            self.finish(channel, error, true);
        }
    }

    fn execute(&mut self, channel: usize, opcode: u8) -> Result<(), u8> {
        let unit = self.channels[channel].selected;
        let packet = matches!(self.drives[channel][unit], Some(Drive::Cdrom(_)));
        match opcode {
            0xec | 0xa1 => {
                if packet != (opcode == 0xa1) {
                    return Err(4);
                }
                let mut data = self.drives[channel][unit]
                    .as_ref()
                    .unwrap()
                    .identify(channel * 2 + unit, Transport::Ide);
                let u = &self.channels[channel].units[unit];
                if !packet {
                    for (index, value) in [
                        (3, u.heads as u16),
                        (6, u.sectors as u16),
                        (55, u.heads as u16),
                        (56, u.sectors as u16),
                        (
                            59,
                            u16::from(u.multiple) | if u.multiple != 0 { 0x100 } else { 0 },
                        ),
                    ] {
                        data[index * 2..index * 2 + 2].copy_from_slice(&value.to_le_bytes());
                    }
                }
                self.start_read(channel, Payload::Bytes(data), false, 512, false);
            }
            0xa0 if packet => {
                let c = &mut self.channels[channel];
                if c.units[unit].regs[1] & 2 != 0 {
                    return Err(4);
                }
                c.packet_limit = usize::from(u16::from_le_bytes([
                    c.units[unit].regs[4],
                    c.units[unit].regs[5],
                ]));
                if c.packet_limit == 0 {
                    c.packet_limit = 65536;
                }
                c.packet_limit = c.packet_limit.min(65534);
                c.transfer = Transfer {
                    phase: Phase::Packet,
                    unit,
                    packet: true,
                    remaining: 12,
                    block: 12,
                    ..Default::default()
                };
                c.buffer = vec![0; 12];
                c.phase_bytes = 12;
                c.units[unit].regs[2] = 1;
                c.units[unit].status = READY | DRQ;
            }
            0x20 | 0x21 | 0x24 | 0x29 | 0xc4 | 0xc8 | 0xc9 | 0x25 | 0x30 | 0x31 | 0x34 | 0x39
            | 0xc5 | 0xca | 0xcb | 0x35
                if !packet =>
            {
                let write = matches!(
                    opcode,
                    0x30 | 0x31 | 0x34 | 0x39 | 0xc5 | 0xca | 0xcb | 0x35
                );
                let ext = matches!(opcode, 0x24 | 0x29 | 0x25 | 0x34 | 0x39 | 0x35);
                let dma = matches!(opcode, 0xc8 | 0xc9 | 0x25 | 0xca | 0xcb | 0x35);
                let multiple = matches!(opcode, 0x29 | 0xc4 | 0x39 | 0xc5);
                let u = &self.channels[channel].units[unit];
                if multiple && u.multiple == 0 {
                    return Err(4);
                }
                let block = if multiple {
                    u.multiple as usize * 512
                } else {
                    512
                };
                let (offset, len) = self.range(channel, ext)?;
                if write
                    && matches!(self.drives[channel][unit], Some(Drive::Disk(ref disk)) if disk.image.read_only())
                {
                    return Err(4);
                }
                let c = &mut self.channels[channel];
                c.transfer = Transfer {
                    phase: if dma {
                        if write {
                            Phase::DmaWrite
                        } else {
                            Phase::DmaRead
                        }
                    } else if write {
                        Phase::Write
                    } else {
                        Phase::Read
                    },
                    unit,
                    offset,
                    remaining: len,
                    block,
                    packet: false,
                    ext,
                    bytes: None,
                };
                if dma {
                    c.units[unit].status = READY | DRQ;
                    self.run_dma(channel);
                } else if write {
                    self.write_phase(channel);
                } else {
                    self.read_phase(channel)?;
                }
            }
            0x40..=0x42 if !packet => {
                let (offset, len) = self.range(channel, opcode == 0x42)?;
                self.channels[channel].transfer.offset = offset + len as u64;
                self.channels[channel].transfer.ext = opcode == 0x42;
                self.advance_address(channel);
                self.finish(channel, 0, true);
            }
            0xc6 if !packet => {
                let count = self.channels[channel].units[unit].regs[2];
                if count != 0 && (!count.is_power_of_two() || count > 16) {
                    return Err(4);
                }
                self.channels[channel].units[unit].multiple = count;
                self.finish(channel, 0, true);
            }
            0x91 if !packet => {
                let u = &mut self.channels[channel].units[unit];
                if u.regs[2] == 0 {
                    return Err(4);
                }
                u.heads = (u.regs[6] & 15) + 1;
                u.sectors = u.regs[2];
                self.finish(channel, 0, true);
            }
            0xf8 | 0x27 if !packet => {
                let sectors = match self.drives[channel][unit].as_ref().unwrap() {
                    Drive::Disk(d) => d.image.size() / 512,
                    _ => unreachable!(),
                };
                let lba = sectors - 1;
                let u = &mut self.channels[channel].units[unit];
                u.regs[3] = lba as u8;
                u.regs[4] = (lba >> 8) as u8;
                u.regs[5] = (lba >> 16) as u8;
                if opcode == 0x27 {
                    u.hob[3] = (lba >> 24) as u8;
                    u.hob[4] = (lba >> 32) as u8;
                    u.hob[5] = (lba >> 40) as u8;
                } else {
                    u.regs[6] = (u.regs[6] & 0xf0) | ((lba >> 24) as u8 & 15);
                }
                self.finish(channel, 0, true);
            }
            0xf9 | 0x37 if !packet => {
                let u = &self.channels[channel].units[unit];
                if u.regs[1] != 0 {
                    return Err(4);
                }
                let mut lba =
                    u64::from(u.regs[3]) | u64::from(u.regs[4]) << 8 | u64::from(u.regs[5]) << 16;
                if opcode == 0x37 {
                    lba |= u64::from(u.hob[3]) << 24
                        | u64::from(u.hob[4]) << 32
                        | u64::from(u.hob[5]) << 40;
                } else {
                    lba |= u64::from(u.regs[6] & 15) << 24;
                }
                let sectors = match self.drives[channel][unit].as_ref().unwrap() {
                    Drive::Disk(d) => d.image.size() / 512,
                    _ => unreachable!(),
                };
                if lba >= sectors {
                    return Err(0x10);
                }
                self.channels[channel].units[unit].limit = lba + 1;
                self.finish(channel, 0, true);
            }
            0xe7 | 0xea if !packet => {
                self.drives[channel][unit].as_mut().unwrap().flush()?;
                self.finish(channel, 0, true);
            }
            0xef => {
                let u = &self.channels[channel].units[unit];
                let (feature, mode) = (u.regs[1], u.regs[2]);
                self.drives[channel][unit]
                    .as_mut()
                    .unwrap()
                    .set_features(feature, mode)?;
                self.finish(channel, 0, true);
            }
            0x90 => {
                for slot in 0..2 {
                    self.reset_unit(channel, slot);
                }
                self.finish(channel, 0, true);
                self.channels[channel].units[unit].error = 1;
            }
            0x08 if packet => {
                self.reset_unit(channel, unit);
                self.finish(channel, 0, false);
            }
            0xe5 if !packet => {
                self.channels[channel].units[unit].regs[2] = 0xff;
                self.finish(channel, 0, true);
            }
            0x10..=0x1f | 0x70..=0x7f | 0xe0..=0xe3 | 0xe6 if !packet => {
                self.finish(channel, 0, true)
            }
            _ => return Err(4),
        }
        Ok(())
    }

    fn start_read(
        &mut self,
        channel: usize,
        payload: Payload,
        packet: bool,
        block: usize,
        dma: bool,
    ) {
        let len = payload.len();
        if len > MAX_TRANSFER {
            self.finish(channel, 4, true);
            return;
        }
        let (offset, bytes) = match payload {
            Payload::Read { offset, .. } => (offset, None),
            Payload::Bytes(bytes) => (0, Some(bytes)),
        };
        let unit = self.channels[channel].selected;
        self.channels[channel].transfer = Transfer {
            phase: if dma { Phase::DmaRead } else { Phase::Read },
            unit,
            offset,
            remaining: len,
            block: block.max(1),
            packet,
            ext: false,
            bytes,
        };
        if len == 0 {
            self.finish(channel, 0, true);
        } else if dma {
            self.channels[channel].units[unit].status = READY | DRQ;
            self.run_dma(channel);
        } else if let Err(error) = self.read_phase(channel) {
            self.finish(channel, error, true);
        }
    }

    fn read_phase(&mut self, channel: usize) -> Result<(), u8> {
        let c = &mut self.channels[channel];
        let t = &c.transfer;
        let mut len = t.remaining.min(t.block);
        if t.packet && len < t.remaining {
            len &= !1;
        }
        if len == 0 {
            return Err(4);
        }
        c.buffer = vec![0; len.next_multiple_of(2)];
        c.phase_bytes = len;
        c.position = 0;
        if let Some(bytes) = &t.bytes {
            c.buffer[..len].copy_from_slice(&bytes[t.offset as usize..t.offset as usize + len]);
        } else {
            self.drives[channel][t.unit]
                .as_mut()
                .ok_or(4u8)?
                .read(t.offset, &mut c.buffer[..len])?;
        }
        c.units[t.unit].status = READY | DRQ;
        if t.packet {
            c.units[t.unit].regs[2] = 2;
            c.units[t.unit].regs[4] = len as u8;
            c.units[t.unit].regs[5] = (len >> 8) as u8;
        }
        self.interrupt(channel);
        Ok(())
    }

    fn write_phase(&mut self, channel: usize) {
        let c = &mut self.channels[channel];
        let len = c.transfer.remaining.min(c.transfer.block);
        c.buffer = vec![0; len];
        c.phase_bytes = len;
        c.position = 0;
        c.units[c.transfer.unit].status = READY | DRQ;
    }

    fn read_data(&mut self, channel: usize, data: &mut [u8]) {
        data.fill(0xff);
        let mut copied = 0;
        while copied < data.len() && self.channels[channel].transfer.phase == Phase::Read {
            let n = self.read_data_step(channel, &mut data[copied..]);
            if n == 0 {
                break;
            }
            copied += n;
        }
    }

    fn read_data_step(&mut self, channel: usize, data: &mut [u8]) -> usize {
        let c = &mut self.channels[channel];
        let n = data.len().min(c.buffer.len() - c.position);
        data[..n].copy_from_slice(&c.buffer[c.position..c.position + n]);
        c.position += n;
        if c.position != c.buffer.len() {
            return n;
        }
        let done = c.phase_bytes;
        c.transfer.remaining -= done;
        c.transfer.offset += done as u64;
        let packet = c.transfer.packet;
        let more = c.transfer.remaining != 0;
        self.advance_address(channel);
        if more {
            if let Err(error) = self.read_phase(channel) {
                self.finish(channel, error, true);
            }
        } else {
            self.finish(channel, 0, packet);
        }
        n
    }

    fn write_data(&mut self, channel: usize, data: &[u8]) {
        let mut copied = 0;
        while copied < data.len()
            && matches!(
                self.channels[channel].transfer.phase,
                Phase::Write | Phase::Packet
            )
        {
            let n = self.write_data_step(channel, &data[copied..]);
            if n == 0 {
                break;
            }
            copied += n;
        }
    }

    fn write_data_step(&mut self, channel: usize, data: &[u8]) -> usize {
        let c = &mut self.channels[channel];
        let phase = c.transfer.phase;
        let n = data.len().min(c.buffer.len() - c.position);
        c.buffer[c.position..c.position + n].copy_from_slice(&data[..n]);
        c.position += n;
        if c.position != c.buffer.len() {
            return n;
        }
        let unit = c.transfer.unit;
        if phase == Phase::Packet {
            let mut cdb = [0; 16];
            cdb[..12].copy_from_slice(&c.buffer);
            let dma = c.units[unit].regs[1] & 1 != 0;
            let block = c.packet_limit;
            let result = match self.drives[channel][unit].as_mut() {
                Some(Drive::Cdrom(optical)) => optical.packet(&cdb),
                _ => Err(4),
            };
            match result {
                Ok(payload) => self.start_read(channel, payload, true, block, dma),
                Err(error) => self.finish(channel, error, true),
            }
            return n;
        }
        let done = c.phase_bytes;
        let result = self.drives[channel][unit].as_mut().unwrap().transfer(
            c.transfer.offset,
            done,
            Some(&c.buffer),
            false,
        );
        if let Err(error) = result {
            self.finish(channel, error, true);
            return n;
        }
        let c = &mut self.channels[channel];
        c.transfer.remaining -= done;
        c.transfer.offset += done as u64;
        let more = c.transfer.remaining != 0;
        self.advance_address(channel);
        if more {
            self.write_phase(channel);
            self.interrupt(channel);
        } else {
            self.finish(channel, 0, true);
        }
        n
    }

    fn prds(&self, channel: usize, length: usize, write: bool) -> Result<Vec<(u64, usize)>, ()> {
        let c = &self.channels[channel];
        let mut table = u64::from(c.prd);
        let table_end = (table | 0xffff) + 1;
        let mut remaining = length;
        let mut regions = Vec::new();
        while remaining != 0 {
            if table + 8 > table_end {
                return Err(());
            }
            let mut prd = [0; 8];
            self.memory
                .read_slice(&mut prd, GuestAddress(table))
                .map_err(|_| ())?;
            table += 8;
            let address = u64::from(u32::from_le_bytes(prd[..4].try_into().unwrap()));
            let count = u16::from_le_bytes(prd[4..6].try_into().unwrap()) as usize;
            let count = if count == 0 { 65536 } else { count };
            if address & 1 != 0
                || count & 1 != 0
                || (address & 0xffff) + count as u64 > 65536
                || prd[6] != 0
                || prd[7] & 0x7f != 0
            {
                return Err(());
            }
            let n = remaining.min(count);
            if !self.memory.check_range(
                GuestAddress(address),
                n,
                if write {
                    Permissions::Read
                } else {
                    Permissions::Write
                },
            ) {
                return Err(());
            }
            regions.push((address, n));
            remaining -= n;
            if prd[7] & 0x80 != 0 && remaining != 0 {
                return Err(());
            }
        }
        Ok(regions)
    }

    fn run_dma(&mut self, channel: usize) {
        let c = &self.channels[channel];
        let phase = c.transfer.phase;
        if !matches!(phase, Phase::DmaRead | Phase::DmaWrite) || c.bm_command & 1 == 0 {
            return;
        }
        self.channels[channel].bm_status |= 1;
        let result = self.dma_transfer(channel);
        self.channels[channel].bm_status &= !1;
        self.channels[channel].bm_status |= 4 | if result.is_err() { 2 } else { 0 };
        self.finish(channel, result.err().unwrap_or(0), true);
    }

    fn dma_transfer(&mut self, channel: usize) -> Result<(), u8> {
        let c = &self.channels[channel];
        let write = c.transfer.phase == Phase::DmaWrite;
        if self.config[4] & 4 == 0 || (c.bm_command & 8 != 0) == write {
            return Err(4);
        }
        let regions = self
            .prds(channel, c.transfer.remaining, write)
            .map_err(|_| 4u8)?;
        let unit = c.transfer.unit;
        let mut offset = c.transfer.offset;
        for (address, len) in regions {
            let mut buffer = vec![0; len];
            if write {
                self.memory
                    .read_slice(&mut buffer, GuestAddress(address))
                    .map_err(|_| 4u8)?;
                self.drives[channel][unit].as_mut().ok_or(4u8)?.transfer(
                    offset,
                    len,
                    Some(&buffer),
                    false,
                )?;
            } else {
                if let Some(bytes) = &self.channels[channel].transfer.bytes {
                    buffer.copy_from_slice(&bytes[offset as usize..offset as usize + len]);
                } else {
                    self.drives[channel][unit]
                        .as_mut()
                        .ok_or(4u8)?
                        .read(offset, &mut buffer)?;
                }
                self.memory
                    .write_slice(&buffer, GuestAddress(address))
                    .map_err(|_| 4u8)?;
            }
            offset += len as u64;
        }
        let c = &mut self.channels[channel];
        c.transfer.offset = offset;
        c.transfer.remaining = 0;
        self.advance_address(channel);
        Ok(())
    }

    fn drive_state(drive: &Option<Drive>) -> Vec<u8> {
        let mut bytes = vec![0; 32];
        match drive {
            None => {}
            Some(Drive::Disk(disk)) => {
                bytes[0] = 1;
                bytes[1] = u8::from(disk.image.read_only());
                bytes[2] = u8::from(disk.write_cache);
                bytes[3] = disk.transfer_mode;
                bytes[8..16].copy_from_slice(&disk.image.size().to_le_bytes());
            }
            Some(Drive::Cdrom(optical)) => {
                bytes[0] = 2;
                bytes[1] = u8::from(optical.tray_open);
                bytes[2] = u8::from(optical.locked);
                bytes[3] = u8::from(optical.attention);
                bytes[4] = optical.event;
                bytes[5..8].copy_from_slice(&optical.sense);
                bytes[8..16].copy_from_slice(
                    &optical
                        .image
                        .as_ref()
                        .map_or(0, |image| image.size())
                        .to_le_bytes(),
                );
                bytes[16..24].copy_from_slice(&optical.generation.to_le_bytes());
                bytes[24] = optical.transfer_mode;
            }
        }
        bytes
    }
}

impl PciDevice for Ide {
    fn config_read(&mut self, reg: usize) -> u32 {
        self.config
            .get(reg.saturating_mul(4)..reg.saturating_mul(4).saturating_add(4))
            .map_or(u32::MAX, |bytes| {
                u32::from_le_bytes(bytes.try_into().unwrap())
            })
    }
    fn config_write(&mut self, reg: usize, offset: u64, data: &[u8]) {
        let Some(start) = reg
            .checked_mul(4)
            .and_then(|v| v.checked_add(offset as usize))
        else {
            return;
        };
        for (i, &byte) in data.iter().enumerate() {
            let Some(pos) = start.checked_add(i) else {
                break;
            };
            let mask = match pos {
                4 => 7,
                5 => 1,
                0x20 => 0xf0,
                0x21..=0x23 => 0xff,
                0x40 | 0x42 | 0x44 => 0xff,
                0x41 | 0x43 => 0xf3,
                _ => 0,
            };
            if pos < 256 {
                self.config[pos] = (self.config[pos] & !mask) | byte & mask;
            }
        }
        for channel in 0..2 {
            self.run_dma(channel);
        }
    }
    fn bars(&self) -> Vec<BarInfo> {
        vec![BarInfo {
            index: 4,
            size: 16,
            kind: BarKind::Io,
        }]
    }
    fn bar_read(&mut self, bar: u8, offset: u64, data: &mut [u8]) {
        data.fill(0xff);
        if bar != 4 || offset >= 16 {
            return;
        }
        let channel = offset as usize / 8;
        let c = &self.channels[channel];
        for (i, byte) in data.iter_mut().enumerate() {
            *byte = match offset as usize % 8 + i {
                0 => c.bm_command,
                2 => c.bm_status,
                pos @ 4..=7 => c.prd.to_le_bytes()[pos - 4],
                _ => 0xff,
            };
        }
    }
    fn bar_write(&mut self, bar: u8, offset: u64, data: &[u8]) {
        if bar != 4 || offset >= 16 {
            return;
        }
        let channel = offset as usize / 8;
        let c = &mut self.channels[channel];
        for (i, &byte) in data.iter().enumerate() {
            match offset as usize % 8 + i {
                0 => {
                    c.bm_command = byte & 9;
                    if byte & 1 == 0 {
                        c.bm_status &= !1;
                    }
                }
                2 => c.bm_status = (c.bm_status & !(byte & 6) & !0x60) | byte & 0x60,
                pos @ 4..=7 if c.bm_status & 1 == 0 => {
                    let mut bytes = c.prd.to_le_bytes();
                    bytes[pos - 4] = byte;
                    c.prd = u32::from_le_bytes(bytes) & !3;
                }
                _ => {}
            }
        }
        self.run_dma(channel);
    }
}

impl Snapshot for Ide {
    fn state_id(&self) -> String {
        self.id.clone()
    }
    fn save_state(&self) -> Result<Vec<u8>, DeviceError> {
        serde_json::to_vec(&Saved {
            version: 1,
            config: self.config.to_vec(),
            channels: self.channels.clone(),
            drives: self
                .drives
                .iter()
                .flatten()
                .map(Self::drive_state)
                .collect(),
        })
        .map_err(|e| DeviceError::State(e.to_string()))
    }
    fn restore_state(&mut self, state: &[u8]) -> Result<(), DeviceError> {
        let invalid = || {
            DeviceError::State("IDE snapshot version, topology, transfer or media mismatch".into())
        };
        let saved: Saved = serde_json::from_slice(state).map_err(|_| invalid())?;
        if saved.version != 1
            || saved.config.len() != 256
            || saved.config[..4] != self.config[..4]
            || saved.drives.len() != 4
        {
            return Err(invalid());
        }
        for (drive, bytes) in self.drives.iter().flatten().zip(&saved.drives) {
            if bytes.len() != 32 {
                return Err(invalid());
            }
            let old = Self::drive_state(drive);
            if bytes[0] != old[0]
                || bytes[8..24] != old[8..24]
                || (bytes[0] == 1 && (bytes[1] != old[1] || bytes[2] > 1))
                || (bytes[0] == 2 && bytes[1..4].iter().any(|&v| v > 1))
            {
                return Err(invalid());
            }
        }
        for (index, c) in saved.channels.iter().enumerate() {
            let t = &c.transfer;
            if c.selected >= 2
                || t.unit >= 2
                || t.remaining > MAX_TRANSFER
                || t.block == 0
                || t.block > 65536
                || c.buffer.len() > 65536
                || c.position > c.buffer.len()
                || c.phase_bytes > c.buffer.len()
                || t.bytes.as_ref().is_some_and(|v| v.len() > MAX_TRANSFER)
                || c.units.iter().any(|u| {
                    u.heads == 0
                        || u.heads > 16
                        || u.sectors == 0
                        || u.multiple > 16
                        || (u.multiple != 0 && !u.multiple.is_power_of_two())
                })
                || t.offset.checked_add(t.remaining as u64).is_none()
                || (t.phase != Phase::Idle && self.drives[index][t.unit].is_none())
                || t.bytes
                    .as_ref()
                    .is_some_and(|bytes| t.offset + t.remaining as u64 > bytes.len() as u64)
            {
                return Err(invalid());
            }
            if matches!(t.phase, Phase::Read | Phase::Write | Phase::Packet)
                && (c.phase_bytes == 0
                    || c.phase_bytes > t.remaining
                    || c.buffer.len()
                        != if t.phase == Phase::Read {
                            c.phase_bytes.next_multiple_of(2)
                        } else {
                            c.phase_bytes
                        })
            {
                return Err(invalid());
            }
            if t.phase == Phase::Packet
                && (c.buffer.len() != 12 || t.remaining != 12 || !t.packet || t.bytes.is_some())
            {
                return Err(invalid());
            }
        }
        self.config.copy_from_slice(&saved.config);
        self.channels = saved.channels;
        for (drive, bytes) in self.drives.iter_mut().flatten().zip(saved.drives) {
            match drive {
                Some(Drive::Disk(d)) => {
                    d.write_cache = bytes[2] != 0;
                    d.transfer_mode = bytes[3];
                }
                Some(Drive::Cdrom(o)) => {
                    o.tray_open = bytes[1] != 0;
                    o.locked = bytes[2] != 0;
                    o.attention = bytes[3] != 0;
                    o.event = bytes[4];
                    o.sense.copy_from_slice(&bytes[5..8]);
                    o.transfer_mode = bytes[24];
                }
                None => {}
            }
        }
        for channel in 0..2 {
            self.update_irq(channel);
        }
        Ok(())
    }
}

impl Resettable for Ide {
    fn reset(&mut self) -> Result<(), DeviceError> {
        let initial = Self::new(self.id.clone(), self.memory.clone(), self.irq.clone());
        self.config = initial.config;
        self.channels = initial.channels;
        for channel in 0..2 {
            for unit in 0..2 {
                self.reset_unit(channel, unit);
            }
            self.update_irq(channel);
        }
        Ok(())
    }
}

impl PciSnapshot for Ide {
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
            Err(DeviceError::State("unexpected IDE wrapped state".into()))
        }
    }
}

/// One task-file or alternate-status/control window of a shared IDE function.
pub struct IdePorts {
    controller: Arc<Mutex<Ide>>,
    channel: usize,
    control: bool,
}

impl IdePorts {
    pub fn new(controller: Arc<Mutex<Ide>>, channel: usize, control: bool) -> Self {
        assert!(channel < 2);
        Self {
            controller,
            channel,
            control,
        }
    }
}

impl BusDevice for IdePorts {
    fn read(&mut self, offset: u64, data: &mut [u8]) {
        let mut controller = self.controller.lock();
        if self.control && offset == 0 {
            controller.read_control(self.channel, data);
        } else if !self.control {
            controller.read_taskfile(self.channel, offset, data);
        } else {
            data.fill(0xff);
        }
    }
    fn write(&mut self, offset: u64, data: &[u8]) {
        let mut controller = self.controller.lock();
        if self.control && offset == 0 {
            controller.write_control(self.channel, data);
        } else if !self.control {
            controller.write_taskfile(self.channel, offset, data);
        }
    }
}
