//! AMD Am79C970A PCnet-PCI II, publication 19436 revision E.
//! Software styles 0, 1, 2 and 3 share one checked descriptor walker.

use std::sync::Arc;

use serde::{Deserialize, Serialize};
use vm_memory::{Bytes, GuestAddress};

use super::{Config, Controller, Frames, MAX_FRAME, crc32, le16, le32, read, readable, writable};
use crate::{BarInfo, BarKind, DeviceError, GuestMemory, IrqLine, PciDevice, Resettable, Snapshot};

const IDON: u16 = 0x100;
const TINT: u16 = 0x200;
const RINT: u16 = 0x400;
const MERR: u16 = 0x800;
const MISS: u16 = 0x1000;

#[derive(Serialize, Deserialize)]
struct State {
    version: u8,
    config: Config,
    csr: Vec<u16>,
    bcr: Vec<u16>,
    prom: [u8; 16],
    rap: u8,
    dword: bool,
    link: bool,
    init_pending: bool,
    rx_index: u32,
    tx_index: u32,
    tx: Vec<u8>,
    tx_flags: u16,
    tx_active: bool,
}

struct Descriptor {
    address: u64,
    buffer: u64,
    flags: u16,
    count: u16,
    message: u32,
}

/// PCnet-PCI II with RAP-selected CSRs/BCRs and legacy INTx.
pub struct Pcnet {
    id: String,
    mac: [u8; 6],
    memory: GuestMemory,
    irq: Arc<dyn IrqLine>,
    state: State,
}

impl Pcnet {
    pub fn new(
        id: impl Into<String>,
        mac: [u8; 6],
        memory: GuestMemory,
        irq: Arc<dyn IrqLine>,
    ) -> Self {
        let mut csr = vec![0; 128];
        for (reg, value) in [
            (0, 4),
            (4, 0x115),
            (80, 0x1410),
            (88, 0x1003),
            (89, 0x0262),
            (100, 0x200),
        ] {
            csr[reg] = value;
        }
        for index in 0..3 {
            csr[12 + index] = le16(&mac, index * 2);
        }
        let mut bcr = vec![0; 32];
        for (reg, value) in [
            (0, 5),
            (1, 5),
            (2, 2),
            (4, 0xc0),
            (5, 0x84),
            (6, 0x88),
            (7, 0x90),
            (18, 0x9001),
            (20, 0x200),
            (22, 0xff06),
        ] {
            bcr[reg] = value;
        }
        let mut prom = [0; 16];
        prom[..6].copy_from_slice(&mac);
        prom[9] = 0x11;
        prom[14..16].copy_from_slice(&[0x57, 0x57]);
        let sum = prom.iter().map(|v| u16::from(*v)).sum::<u16>();
        prom[12..14].copy_from_slice(&sum.to_le_bytes());
        irq.set_level(false);
        Self {
            id: id.into(),
            mac,
            memory,
            irq,
            state: State {
                version: 1,
                config: Config::new(true),
                csr,
                bcr,
                prom,
                rap: 0,
                dword: false,
                link: false,
                init_pending: false,
                rx_index: 0,
                tx_index: 0,
                tx: Vec::new(),
                tx_flags: 0,
                tx_active: false,
            },
        }
    }

    fn style(&self) -> u8 {
        self.state.bcr[20] as u8
    }
    fn wide(&self) -> bool {
        self.state.bcr[20] & 0x100 != 0
    }
    fn stopped(&self) -> bool {
        self.state.csr[0] & 4 != 0 || self.state.csr[5] & 1 != 0
    }
    fn upper(&self) -> u64 {
        if self.wide() {
            0
        } else {
            u64::from(self.state.csr[2] & 0xff00) << 16
        }
    }
    fn update_irq(&mut self) {
        let pending = self.state.csr[0] & !self.state.csr[3] & 0x7f00 != 0
            || self.state.csr[4] & !(self.state.csr[4] << 1) & 0x026a != 0
            || self.state.csr[5] & (self.state.csr[5] >> 1) & 0x0a90 != 0;
        self.state.csr[0] = (self.state.csr[0] & !0x8080)
            | if pending { 0x80 } else { 0 }
            | if self.state.csr[0] & 0x7800 != 0 {
                0x8000
            } else {
                0
            };
        self.irq
            .set_level(pending && self.state.csr[0] & 0x40 != 0 && self.state.config.intx());
    }
    fn cause(&mut self, value: u16) {
        self.state.csr[0] |= value;
        self.update_irq();
    }
    fn dma_error(&mut self) {
        self.state.config.bytes[7] |= 0x20;
        self.state.csr[0] &= !0x30;
        self.cause(MERR);
    }

    fn csr_read(&mut self) -> u16 {
        let reg = usize::from(self.state.rap);
        match reg {
            0 => {
                self.update_irq();
                self.state.csr[0]
            }
            58 => self.state.bcr[20],
            16 => self.state.csr[1],
            17 => self.state.csr[2],
            _ => self.state.csr[reg],
        }
    }

    fn style_write(&mut self, value: u16) {
        if !self.stopped() || value as u8 > 3 {
            return;
        }
        self.state.bcr[20] = match value as u8 {
            0 => 0x200,
            1 => 0x101,
            2 => 0x302,
            3 => 0x303,
            _ => unreachable!(),
        };
    }

    fn csr_write(&mut self, value: u16) {
        let reg = usize::from(self.state.rap);
        match reg {
            0 => {
                self.state.csr[0] &= !(value & 0x7f00);
                self.state.csr[0] = (self.state.csr[0] & !0x40) | (value & 0x40);
                if value & 4 != 0 {
                    self.state.csr[0] = 4;
                    self.state.init_pending = false;
                    self.state.tx.clear();
                    self.state.tx_active = false;
                } else {
                    if value & 1 != 0 {
                        self.state.csr[0] = (self.state.csr[0] & !4) | 1;
                        self.state.init_pending = true;
                    }
                    if value & 2 != 0 {
                        self.state.csr[0] = (self.state.csr[0] & !4) | 2;
                        if self.state.csr[15] & 1 == 0 {
                            self.state.csr[0] |= 0x20;
                        }
                        if self.state.csr[15] & 2 == 0 {
                            self.state.csr[0] |= 0x10;
                        }
                    }
                }
            }
            3 => self.state.csr[3] = value,
            4 => {
                self.state.csr[4] =
                    (self.state.csr[4] & !(value & 0x026a) & 0x026a) | (value & !0x026a)
            }
            5 => {
                self.state.csr[5] =
                    (self.state.csr[5] & !(value & 0x0a90) & 0x0a90) | (value & !0x0a90)
            }
            58 => self.style_write(value),
            88 | 89 => {}
            1 | 2 | 8..=15 | 24 | 25 | 30 | 31 | 76 | 78 if !self.stopped() => {}
            _ => self.state.csr[reg] = value,
        }
        self.update_irq();
    }

    fn bcr_read(&self) -> u16 {
        let reg = usize::from(self.state.rap);
        let Some(&value) = self.state.bcr.get(reg) else {
            return 0xffff;
        };
        if (4..=7).contains(&reg) {
            (value & !0x8000) | if self.state.link { 0x8000 } else { 0 }
        } else {
            value
        }
    }
    fn bcr_write(&mut self, value: u16) {
        let reg = usize::from(self.state.rap);
        if reg >= self.state.bcr.len() {
            return;
        }
        match reg {
            20 => self.style_write(value),
            18 => {
                self.state.bcr[18] = (value & !0x80) | if self.state.dword { 0x80 } else { 0 };
            }
            22 => {}
            _ => self.state.bcr[reg] = value,
        }
    }

    fn initialize(&mut self) {
        if !self.state.init_pending || !self.state.config.dma() {
            return;
        }
        let address = u64::from(self.state.csr[1]) | u64::from(self.state.csr[2]) << 16;
        let wide = self.wide();
        let mut block = [0; 28];
        let length = if wide { 28 } else { 24 };
        if !readable(&self.memory, address, length)
            || self
                .memory
                .read_slice(&mut block[..length], GuestAddress(address))
                .is_err()
        {
            self.state.init_pending = false;
            self.dma_error();
            return;
        }
        self.state.csr[15] = le16(&block, 0);
        let (mac_offset, filter_offset, rx, tx, rlog, tlog) = if wide {
            (
                4,
                12,
                le32(&block, 20),
                le32(&block, 24),
                (block[2] >> 4) & 15,
                (block[3] >> 4) & 15,
            )
        } else {
            (
                2,
                8,
                le32(&block, 16) & 0xffffff,
                le32(&block, 20) & 0xffffff,
                block[19] >> 5,
                block[23] >> 5,
            )
        };
        if rlog > 9 || tlog > 9 {
            self.state.init_pending = false;
            self.dma_error();
            return;
        }
        for i in 0..3 {
            self.state.csr[12 + i] = le16(&block, mac_offset + i * 2);
        }
        for i in 0..4 {
            self.state.csr[8 + i] = le16(&block, filter_offset + i * 2);
        }
        self.state.csr[24] = rx as u16;
        self.state.csr[25] = (rx >> 16) as u16;
        self.state.csr[30] = tx as u16;
        self.state.csr[31] = (tx >> 16) as u16;
        self.state.csr[6] = u16::from(tlog) << 12 | u16::from(rlog) << 8;
        self.state.csr[76] = (1u16 << rlog).wrapping_neg();
        self.state.csr[78] = (1u16 << tlog).wrapping_neg();
        self.state.rx_index = 0;
        self.state.tx_index = 0;
        self.state.init_pending = false;
        self.state.csr[0] &= !1;
        if self.state.csr[0] & 2 != 0 {
            self.state.csr[0] &= !0x30;
            if self.state.csr[15] & 1 == 0 {
                self.state.csr[0] |= 0x20;
            }
            if self.state.csr[15] & 2 == 0 {
                self.state.csr[0] |= 0x10;
            }
        }
        self.cause(IDON);
    }

    fn ring(&self, receive: bool) -> Option<(u64, u32)> {
        let (reg, len) = if receive { (24, 76) } else { (30, 78) };
        let base = u64::from(self.state.csr[reg])
            | u64::from(self.state.csr[reg + 1]) << 16
            | self.upper();
        let length = u32::from(self.state.csr[len].wrapping_neg());
        if length == 0 || length > 512 {
            None
        } else {
            Some((base, length))
        }
    }

    fn descriptor(&self, base: u64, index: u32) -> Option<Descriptor> {
        let wide = self.wide();
        let address = base.checked_add(u64::from(index) * if wide { 16 } else { 8 })?;
        if wide {
            let bytes = read::<16>(&self.memory, address)?;
            let swapped = self.style() == 3;
            Some(Descriptor {
                address,
                buffer: u64::from(le32(&bytes, if swapped { 8 } else { 0 })),
                flags: le16(&bytes, 6),
                count: le16(&bytes, 4),
                message: le32(&bytes, if swapped { 0 } else { 8 }),
            })
        } else {
            let bytes = read::<8>(&self.memory, address)?;
            Some(Descriptor {
                address,
                buffer: u64::from(le32(&bytes, 0) & 0xffffff) | self.upper(),
                flags: u16::from(bytes[3]) << 8,
                count: le16(&bytes, 4),
                message: u32::from(le16(&bytes, 6)),
            })
        }
    }

    fn completion_writable(&self, descriptor: &Descriptor) -> bool {
        writable(
            &self.memory,
            descriptor.address,
            if self.wide() { 16 } else { 8 },
        )
    }

    fn complete(&mut self, descriptor: &Descriptor, flags: u16, message: u32) {
        if self.wide() {
            let offset = if self.style() == 3 { 0 } else { 8 };
            let _ = self.memory.write_slice(
                &message.to_le_bytes(),
                GuestAddress(descriptor.address + offset),
            );
            let _ = self.memory.write_slice(
                &(flags & !0x8000).to_le_bytes(),
                GuestAddress(descriptor.address + 6),
            );
        } else {
            let _ = self.memory.write_slice(
                &(message as u16).to_le_bytes(),
                GuestAddress(descriptor.address + 6),
            );
            let _ = self.memory.write_slice(
                &[(flags >> 8) as u8 & 0x7f],
                GuestAddress(descriptor.address + 3),
            );
        }
    }

    fn swap_data(&self, packet: &mut [u8]) {
        if self.state.csr[3] & 4 != 0 {
            for pair in packet.as_chunks_mut::<2>().0 {
                pair.swap(0, 1);
            }
        }
    }

    fn tx(&mut self, frames: &mut Frames) {
        if self.state.csr[0] & 0x10 == 0 || self.stopped() || !self.state.config.dma() {
            return;
        }
        let Some((base, count)) = self.ring(false) else {
            return;
        };
        for _ in 0..count.min(64) {
            if frames.tx.len() >= 64 {
                break;
            }
            let Some(desc) = self.descriptor(base, self.state.tx_index % count) else {
                self.dma_error();
                break;
            };
            self.state.csr[34] = desc.buffer as u16;
            self.state.csr[35] = (desc.buffer >> 16) as u16;
            self.state.csr[40] = desc.count;
            self.state.csr[42] = desc.flags;
            if desc.flags & 0x8000 == 0 {
                if self.state.tx_active {
                    if let Some(previous) =
                        self.descriptor(base, (self.state.tx_index + count - 1) % count)
                        && self.completion_writable(&previous)
                    {
                        self.complete(
                            &previous,
                            previous.flags | 0x4000,
                            previous.message | 0xc000_0000,
                        );
                    }
                    self.state.tx_active = false;
                    self.state.tx.clear();
                    if self.state.csr[3] & 0x40 == 0 {
                        self.state.csr[0] &= !0x10;
                    }
                    self.cause(TINT);
                }
                break;
            }
            if !self.completion_writable(&desc) {
                self.dma_error();
                break;
            }
            if desc.flags & 0x200 != 0 {
                self.state.tx.clear();
                self.state.tx_active = true;
                self.state.tx_flags = desc.flags;
            }
            let length = usize::from(desc.count.wrapping_neg() & 0xfff);
            let length = if length == 0 { 4096 } else { length };
            let mut failed = false;
            if !self.state.tx_active
                || self.state.tx.len().saturating_add(length) > MAX_FRAME
                || desc.count & 0xf000 != 0xf000
                || !readable(&self.memory, desc.buffer, length)
            {
                failed = true;
            } else {
                let start = self.state.tx.len();
                self.state.tx.resize(start + length, 0);
                if self
                    .memory
                    .read_slice(&mut self.state.tx[start..], GuestAddress(desc.buffer))
                    .is_err()
                {
                    failed = true;
                }
            }
            let mut flags = desc.flags;
            let mut message = desc.message;
            if failed {
                flags |= 0x4000;
                message |= 0xc000_0000;
                self.state.tx.clear();
                self.state.tx_active = false;
                self.dma_error();
            } else if desc.flags & 0x100 != 0 {
                let mut packet = std::mem::take(&mut self.state.tx);
                self.swap_data(&mut packet);
                if self.state.csr[4] & 0x800 != 0 && packet.len() < 60 {
                    packet.resize(60, 0);
                }
                if self.state.csr[15] & 4 != 0 {
                    frames.receive(packet);
                } else if !self.state.link {
                    flags |= 0x4000;
                    message |= 0x0800_0000;
                } else {
                    frames.transmit(packet);
                }
                self.state.tx_active = false;
            }
            self.complete(&desc, flags, message);
            self.state.tx_index = (self.state.tx_index + 1) % count;
            self.state.csr[74] = count.saturating_sub(self.state.tx_index) as u16;
            if failed
                || (desc.flags & 0x100 != 0 && self.state.csr[5] & 0x8000 == 0)
                || (desc.flags & 0x1000 != 0 && self.state.csr[5] & 0x4000 != 0)
            {
                self.cause(TINT);
            }
        }
    }

    fn accepts(&self, packet: &[u8]) -> Option<u16> {
        if packet.len() < 14 {
            return None;
        }
        let mode = self.state.csr[15];
        if packet[..6] == [0xff; 6] {
            if mode & 0x4000 == 0 {
                return Some(0x10);
            }
        } else if packet[0] & 1 == 0 && mode & 0x2000 == 0 {
            let mac: Vec<u8> = self.state.csr[12..15]
                .iter()
                .flat_map(|v| v.to_le_bytes())
                .collect();
            if packet[..6] == mac {
                return Some(0x40);
            }
        } else if packet[0] & 1 != 0 {
            // The reflected CRC's high six bits index the logical filter.
            let hash = ((!crc32(&packet[..6])).reverse_bits() >> 26) as usize;
            if self.state.csr[8 + hash / 16] & (1 << (hash % 16)) != 0 {
                return Some(0x20);
            }
        }
        if mode & 0x8000 != 0 { Some(0) } else { None }
    }

    fn rx(&mut self, frames: &mut Frames) {
        if self.state.csr[0] & 0x20 == 0 || self.stopped() || !self.state.config.dma() {
            frames.rx.clear();
            return;
        }
        for _ in 0..64 {
            let Some(mut packet) = frames.rx.pop_front() else {
                break;
            };
            let Some(matched) = self.accepts(&packet) else {
                continue;
            };
            packet.resize(packet.len().max(60), 0);
            let strip = self.state.csr[4] & 0x400 != 0
                && packet.len() >= 14
                && u16::from_be_bytes([packet[12], packet[13]]) <= 1500;
            if strip {
                packet.truncate(
                    (14 + usize::from(u16::from_be_bytes([packet[12], packet[13]])))
                        .min(packet.len()),
                );
            } else {
                packet.extend_from_slice(&crc32(&packet).to_le_bytes());
            }
            self.swap_data(&mut packet);
            let Some((base, count)) = self.ring(true) else {
                self.cause(MISS);
                continue;
            };
            let mut chain = Vec::new();
            let mut length = 0;
            for index in 0..count {
                let Some(desc) = self.descriptor(base, (self.state.rx_index + index) % count)
                else {
                    self.dma_error();
                    break;
                };
                if desc.flags & 0x8000 == 0 {
                    break;
                }
                let capacity = usize::from(desc.count.wrapping_neg() & 0xfff);
                let capacity = if capacity == 0 { 4096 } else { capacity };
                let n = capacity.min(packet.len() - length);
                if desc.count & 0xf000 != 0xf000
                    || !self.completion_writable(&desc)
                    || !writable(&self.memory, desc.buffer, n)
                {
                    self.dma_error();
                    break;
                }
                chain.push((desc, n));
                length += n;
                if length == packet.len() {
                    break;
                }
            }
            if chain.is_empty() {
                self.cause(MISS);
                continue;
            }
            let complete = length == packet.len();
            let mut offset = 0;
            let last = chain.len() - 1;
            for (index, (desc, length)) in chain.into_iter().enumerate() {
                let _ = self
                    .memory
                    .write_slice(&packet[offset..offset + length], GuestAddress(desc.buffer));
                offset += length;
                let flags = (if index == 0 { 0x200 } else { 0 })
                    | if index == last {
                        if complete { 0x100 | matched } else { 0x4c00 }
                    } else {
                        0
                    };
                let message = if index == last && complete {
                    packet.len() as u32 & 0xfff
                } else {
                    0
                };
                self.complete(&desc, flags, message);
                self.state.rx_index = (self.state.rx_index + 1) % count;
            }
            self.state.csr[72] = count.saturating_sub(self.state.rx_index) as u16;
            if !complete {
                self.cause(MISS);
            }
            self.cause(RINT);
        }
    }

    fn soft_reset(&mut self) {
        let config = self.state.config.clone();
        let bcr = self.state.bcr.clone();
        let link = self.state.link;
        self.state = Self::new(
            self.id.clone(),
            self.mac,
            self.memory.clone(),
            self.irq.clone(),
        )
        .state;
        self.state.config = config;
        self.state.bcr = bcr;
        self.state.bcr[18] &= !0x80;
        self.state.link = link;
    }
}

impl Controller for Pcnet {
    fn service(&mut self, _elapsed_ns: u64, frames: &mut Frames) {
        self.initialize();
        self.tx(frames);
        self.rx(frames);
    }
    fn set_link(&mut self, up: bool) {
        self.state.link = up;
    }
}

impl PciDevice for Pcnet {
    fn config_read(&mut self, reg: usize) -> u32 {
        self.state.config.read(reg)
    }
    fn config_write(&mut self, reg: usize, offset: u64, data: &[u8]) {
        self.state.config.write(reg, offset, data);
        self.update_irq();
    }
    fn bars(&self) -> Vec<BarInfo> {
        vec![
            BarInfo {
                index: 0,
                size: 32,
                kind: BarKind::Io,
            },
            BarInfo {
                index: 1,
                size: 32,
                kind: BarKind::Mmio32,
            },
        ]
    }
    fn bar_read(&mut self, bar: u8, offset: u64, data: &mut [u8]) {
        data.fill(0xff);
        if bar > 1 {
            return;
        }
        if offset < 16 && offset + data.len() as u64 <= 16 {
            data.copy_from_slice(&self.state.prom[offset as usize..offset as usize + data.len()]);
            return;
        }
        let width = if self.state.dword { 4 } else { 2 };
        if data.len() != width || offset & (width as u64 - 1) != 0 {
            return;
        }
        let index = (offset.saturating_sub(0x10)) / width as u64;
        let value = match index {
            0 => self.csr_read(),
            1 => u16::from(self.state.rap),
            2 => {
                self.soft_reset();
                0
            }
            3 => self.bcr_read(),
            _ => 0xffff,
        };
        data.fill(0);
        data[..2].copy_from_slice(&value.to_le_bytes());
    }
    fn bar_write(&mut self, bar: u8, offset: u64, data: &[u8]) {
        if bar > 1 {
            return;
        }
        if !self.state.dword && offset == 0x10 && data.len() == 4 {
            self.state.dword = true;
            self.state.bcr[18] |= 0x80;
            return;
        }
        let width = if self.state.dword { 4 } else { 2 };
        if data.len() != width || offset < 0x10 || offset & (width as u64 - 1) != 0 {
            return;
        }
        let value = le16(data, 0);
        match (offset - 0x10) / width as u64 {
            0 => self.csr_write(value),
            1 => self.state.rap = (value & 0x7f) as u8,
            2 => {}
            3 => self.bcr_write(value),
            _ => {}
        }
    }
}

impl Resettable for Pcnet {
    fn reset(&mut self) -> Result<(), DeviceError> {
        self.state = Self::new(
            self.id.clone(),
            self.mac,
            self.memory.clone(),
            self.irq.clone(),
        )
        .state;
        Ok(())
    }
}

impl Snapshot for Pcnet {
    fn state_id(&self) -> String {
        self.id.clone()
    }
    fn save_state(&self) -> Result<Vec<u8>, DeviceError> {
        serde_json::to_vec(&self.state).map_err(|e| DeviceError::State(e.to_string()))
    }
    fn restore_state(&mut self, state: &[u8]) -> Result<(), DeviceError> {
        let state: State =
            serde_json::from_slice(state).map_err(|e| DeviceError::State(e.to_string()))?;
        if state.version != 1
            || state.config.bytes.len() != 256
            || state.csr.len() != 128
            || state.bcr.len() != 32
            || state.rap >= 128
            || state.tx.len() > MAX_FRAME
            || state.bcr[20] as u8 > 3
            || state.rx_index > 512
            || state.tx_index > 512
        {
            return Err(DeviceError::State("invalid pcnet state".into()));
        }
        self.state = state;
        self.update_irq();
        Ok(())
    }
}

#[cfg(test)]
mod tests;
