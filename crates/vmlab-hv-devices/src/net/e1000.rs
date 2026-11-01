//! Intel 82540EM, specified by the PCI/PCI-X GbE SDM, revision 4.0.
//! Register offsets and descriptor layouts are from chapters 3, 5 and 13.

use std::sync::Arc;

use serde::{Deserialize, Serialize};
use vm_memory::{Bytes, GuestAddress};

use super::{
    Config, Controller, Frames, MAX_FRAME, checksum, crc32, le16, le32, le64, read, readable,
    writable,
};
use crate::{BarInfo, BarKind, DeviceError, GuestMemory, IrqLine, PciDevice, Resettable, Snapshot};

const CTRL: usize = 0;
const STATUS: usize = 8;
const EECD: usize = 0x10;
const EERD: usize = 0x14;
const MDIC: usize = 0x20;
const ICR: usize = 0xc0;
const ITR: usize = 0xc4;
const IMS: usize = 0xd0;
const RCTL: usize = 0x100;
const TCTL: usize = 0x400;
const RDBAL: usize = 0x2800;
const RDBAH: usize = 0x2804;
const RDLEN: usize = 0x2808;
const RDH: usize = 0x2810;
const RDT: usize = 0x2818;
const RDTR: usize = 0x2820;
const RADV: usize = 0x282c;
const TDBAL: usize = 0x3800;
const TDBAH: usize = 0x3804;
const TDLEN: usize = 0x3808;
const TDH: usize = 0x3810;
const TDT: usize = 0x3818;
const TIDV: usize = 0x3820;
const TADV: usize = 0x382c;
const TXDW: u32 = 1;
const TXQE: u32 = 2;
const LSC: u32 = 4;
const RXDMT0: u32 = 0x10;
const RXO: u32 = 0x40;
const RXT0: u32 = 0x80;

#[derive(Clone, Default, Serialize, Deserialize)]
struct EepromBus {
    command: u16,
    bits: u8,
    output: u16,
    remaining: u8,
}

#[derive(Clone, Default, Serialize, Deserialize)]
struct Delay {
    causes: u32,
    relative: u64,
    absolute: u64,
}

#[derive(Clone, Serialize, Deserialize)]
struct State {
    version: u8,
    config: Config,
    regs: Vec<u32>,
    phy: Vec<u16>,
    eeprom: Vec<u16>,
    serial: EepromBus,
    io_address: u32,
    link: bool,
    tx: Vec<u8>,
    tx_bad: bool,
    tx_options: u8,
    tx_css: u8,
    context: [u8; 16],
    tso_context: [u8; 16],
    rx_delay: Delay,
    tx_delay: Delay,
    throttle: u64,
    irq: bool,
}

/// One Intel PRO/1000 MT PCI function with legacy INTx delivery.
pub struct E1000 {
    id: String,
    mac: [u8; 6],
    memory: GuestMemory,
    irq: Arc<dyn IrqLine>,
    state: State,
}

impl E1000 {
    pub fn new(
        id: impl Into<String>,
        mac: [u8; 6],
        memory: GuestMemory,
        irq: Arc<dyn IrqLine>,
    ) -> Self {
        let mut regs = vec![0; 0x20000 / 4];
        for (reg, value) in [
            (CTRL, 0x0014_0240),
            (STATUS, 0x8008_0781),
            (EECD, 0x188),
            (0x28, 0x00c2_8001),
            (0x30, 0x8808),
            (0x410, 0x0060_200a),
            (0x1000, 0x0010_0030),
            (0x5820, 0x0022_2300),
        ] {
            regs[reg / 4] = value;
        }
        regs[0x5400 / 4] = le32(&mac, 0);
        regs[0x5404 / 4] = u32::from(le16(&mac, 4)) | 1 << 31;
        let mut eeprom = vec![0; 64];
        for (i, word) in eeprom.iter_mut().take(3).enumerate() {
            *word = le16(&mac, i * 2);
        }
        for (index, value) in [
            (0xa, 0x4408),
            (0xb, 0x1100),
            (0xc, 0x1af4),
            (0xd, 0x100e),
            (0xe, 0x8086),
            (0xf, 0x3040),
            (0x12, 0x1000),
        ] {
            eeprom[index] = value;
        }
        eeprom[63] = 0xbabau16.wrapping_sub(
            eeprom[..63]
                .iter()
                .fold(0u16, |sum, v| sum.wrapping_add(*v)),
        );
        let mut phy = vec![0; 32];
        for (reg, value) in [
            (0, 0x1140),
            (1, 0x7949),
            (2, 0x0141),
            (3, 0x0c20),
            (4, 0x0de1),
            (5, 0x0de1),
            (6, 0xf),
            (9, 0x0e00),
            (10, 0x3c00),
            (16, 0x360),
            (17, 0xac00),
            (20, 0xc00),
        ] {
            phy[reg] = value;
        }
        irq.set_level(false);
        Self {
            id: id.into(),
            mac,
            memory,
            irq,
            state: State {
                version: 1,
                config: Config::new(false),
                regs,
                phy,
                eeprom,
                serial: EepromBus::default(),
                io_address: 0,
                link: false,
                tx: Vec::new(),
                tx_bad: false,
                tx_options: 0,
                tx_css: 0,
                context: [0; 16],
                tso_context: [0; 16],
                rx_delay: Delay::default(),
                tx_delay: Delay::default(),
                throttle: 0,
                irq: false,
            },
        }
    }

    fn reg(&self, reg: usize) -> u32 {
        self.state.regs[reg / 4]
    }
    fn put(&mut self, reg: usize, value: u32) {
        self.state.regs[reg / 4] = value;
    }

    fn update_irq(&mut self) {
        let pending = self.reg(ICR) & self.reg(IMS) != 0 && self.state.config.intx();
        let high = pending && (self.state.irq || self.state.throttle == 0);
        if high && !self.state.irq {
            self.state.throttle = u64::from(self.reg(ITR) & 0xffff) * 256;
        }
        self.state.irq = high;
        self.irq.set_level(high);
    }

    fn cause(&mut self, causes: u32) {
        self.put(ICR, self.reg(ICR) | causes);
        self.update_irq();
    }

    fn delayed(&mut self, causes: u32, receive: bool, enabled: bool) {
        let (relative, absolute) = if receive {
            (self.reg(RDTR) & 0xffff, self.reg(RADV) & 0xffff)
        } else {
            (self.reg(TIDV) & 0xffff, self.reg(TADV) & 0xffff)
        };
        if !enabled || relative == 0 {
            let delay = if receive {
                &mut self.state.rx_delay
            } else {
                &mut self.state.tx_delay
            };
            let all = causes | delay.causes;
            *delay = Delay::default();
            self.cause(all);
        } else {
            let delay = if receive {
                &mut self.state.rx_delay
            } else {
                &mut self.state.tx_delay
            };
            if delay.causes == 0 {
                delay.absolute = u64::from(absolute) * 1024;
            }
            delay.causes |= causes;
            delay.relative = u64::from(relative) * 1024;
        }
    }

    fn tick(&mut self, ns: u64) {
        self.state.throttle = self.state.throttle.saturating_sub(ns);
        let mut causes = 0;
        for delay in [&mut self.state.rx_delay, &mut self.state.tx_delay] {
            if delay.causes == 0 {
                continue;
            }
            let expire = ns >= delay.relative || (delay.absolute != 0 && ns >= delay.absolute);
            delay.relative = delay.relative.saturating_sub(ns);
            delay.absolute = delay.absolute.saturating_sub(ns);
            if expire {
                causes |= delay.causes;
                *delay = Delay::default();
            }
        }
        self.cause(causes);
    }

    fn dma_error(&mut self, receive: bool) {
        self.state.config.bytes[7] |= 0x20;
        self.cause(if receive { RXO } else { TXQE });
    }

    fn eecd_write(&mut self, value: u32) {
        let old = self.reg(EECD);
        let mut value =
            (value & 0x77) | 0x100 | if value & 0x40 != 0 { 0x80 } else { old & 0x80 } | (old & 8);
        if value & 2 == 0 {
            self.state.serial = EepromBus::default();
            value |= 8;
        } else if old & 1 == 0 && value & 1 != 0 {
            let serial = &mut self.state.serial;
            if serial.remaining != 0 {
                value = (value & !8) | (u32::from(serial.output & 0x8000 != 0) * 8);
                serial.output <<= 1;
                serial.remaining -= 1;
                if serial.remaining == 0 {
                    serial.bits = 0;
                    serial.command = 0;
                }
            } else if serial.bits != 0 || value & 4 != 0 {
                serial.command = (serial.command << 1) | u16::from(value & 4 != 0);
                serial.bits += 1;
                if serial.bits == 9 {
                    serial.output = if serial.command >> 6 == 6 {
                        self.state.eeprom[usize::from(serial.command & 63)]
                    } else {
                        0xffff
                    };
                    serial.remaining = 16;
                    value &= !8;
                }
            }
        }
        self.put(EECD, value);
    }

    fn mdic_write(&mut self, value: u32) {
        let reg = ((value >> 16) & 31) as usize;
        let mut result = value | 1 << 28;
        if (value >> 21) & 31 != 1 {
            result |= 1 << 30;
        } else if value & (1 << 27) != 0 {
            result = (result & !0xffff) | u32::from(self.state.phy[reg]);
        } else if value & (1 << 26) != 0 {
            match reg {
                0 => {
                    self.state.phy[0] = (value as u16) & !0x8200;
                    if value & 0x8000 != 0 {
                        self.state.phy[0] = 0x1140;
                    }
                }
                2 | 3 | 1 | 5 | 6 | 10 | 17 => {}
                _ => self.state.phy[reg] = value as u16,
            }
        }
        self.put(MDIC, result);
        if value & (1 << 29) != 0 {
            self.cause(1 << 9);
        }
    }

    fn read_reg(&mut self, offset: usize) -> u32 {
        match offset {
            ICR => {
                let value = self.reg(ICR);
                self.put(ICR, 0);
                self.update_irq();
                value
            }
            0xc8 | 0xd8 => 0,
            0x4000..=0x40fc => {
                let value = self.reg(offset);
                self.put(offset, 0);
                value
            }
            _ => self.state.regs.get(offset / 4).copied().unwrap_or(0),
        }
    }

    fn write_reg(&mut self, offset: usize, value: u32) {
        if offset >= 0x20000 {
            return;
        }
        match offset {
            CTRL if value & (1 << 26) != 0 => {
                self.mac_reset();
            }
            CTRL => self.put(CTRL, value & !(1 << 31)),
            STATUS | 0x4000..=0x40fc => {}
            EECD => self.eecd_write(value),
            EERD => {
                let address = ((value >> 8) & 0xff) as usize;
                self.put(
                    EERD,
                    if value & 1 != 0 {
                        (u32::from(*self.state.eeprom.get(address).unwrap_or(&0xffff)) << 16)
                            | (value & 0xff00)
                            | 0x10
                    } else {
                        value
                    },
                );
            }
            MDIC => self.mdic_write(value),
            ICR => {
                self.put(ICR, self.reg(ICR) & !value);
                self.update_irq();
            }
            0xc8 => self.cause(value & 0x1ffff),
            IMS => {
                self.put(IMS, self.reg(IMS) | (value & 0x1ffff));
                self.update_irq();
            }
            0xd8 => {
                self.put(IMS, self.reg(IMS) & !value);
                self.update_irq();
            }
            RDBAL | TDBAL => self.put(offset, value & !15),
            RDLEN | TDLEN => self.put(offset, value & 0x000f_ff80),
            RDH | RDT | TDH | TDT => self.put(offset, value & 0xffff),
            RDTR if value & (1 << 31) != 0 => {
                self.put(RDTR, value & 0xffff);
                let causes = self.state.rx_delay.causes;
                self.state.rx_delay = Delay::default();
                self.cause(causes);
            }
            _ => self.put(offset, value),
        }
    }

    fn mac_reset(&mut self) {
        let link = self.state.link;
        let config = self.state.config.clone();
        let eeprom = self.state.eeprom.clone();
        let mut initial = Self::new(
            self.id.clone(),
            self.mac,
            self.memory.clone(),
            self.irq.clone(),
        );
        initial.state.config = config;
        initial.state.eeprom = eeprom;
        self.state = initial.state;
        self.set_link(link);
        self.put(ICR, 0);
    }

    fn ring(
        &self,
        base: usize,
        high: usize,
        length: usize,
        head: usize,
        tail: usize,
    ) -> Option<(u64, u32, u32, u32)> {
        let size = self.reg(length) / 16;
        let head = self.reg(head);
        let tail = self.reg(tail);
        if size == 0 || head >= size || tail >= size {
            return None;
        }
        Some((
            u64::from(self.reg(base)) | u64::from(self.reg(high)) << 32,
            size,
            head,
            tail,
        ))
    }

    fn tx(&mut self, frames: &mut Frames) {
        if self.reg(TCTL) & 2 == 0 || !self.state.config.dma() {
            return;
        }
        let Some((base, count, mut head, tail)) = self.ring(TDBAL, TDBAH, TDLEN, TDH, TDT) else {
            return;
        };
        for _ in 0..count.min(256) {
            if head == tail || frames.tx.len() >= 64 {
                break;
            }
            let Some(address) = base.checked_add(u64::from(head) * 16) else {
                self.dma_error(false);
                break;
            };
            let Some(desc) = read::<16>(&self.memory, address) else {
                self.dma_error(false);
                break;
            };
            let command = desc[11];
            let extended = command & 0x20 != 0;
            let context = extended && desc[10] & 0xf0 == 0;
            if command & 8 != 0 && !writable(&self.memory, address + 12, 1) {
                self.dma_error(false);
                break;
            }
            if context {
                if command & 4 != 0 {
                    self.state.tso_context = desc;
                } else {
                    self.state.context = desc;
                }
            } else {
                if self.state.tx.is_empty() {
                    self.state.tx_options = if extended { desc[13] } else { 0 };
                    self.state.tx_css = desc[13];
                }
                let length = if extended {
                    (le32(&desc, 8) & 0xfffff) as usize
                } else {
                    usize::from(le16(&desc, 8))
                };
                let buffer = le64(&desc, 0);
                if buffer != 0 && length != 0 {
                    if self.state.tx.len().saturating_add(length) > MAX_FRAME
                        || !readable(&self.memory, buffer, length)
                    {
                        self.state.tx_bad = true;
                        self.dma_error(false);
                    } else if !self.state.tx_bad {
                        let start = self.state.tx.len();
                        self.state.tx.resize(start + length, 0);
                        if self
                            .memory
                            .read_slice(&mut self.state.tx[start..], GuestAddress(buffer))
                            .is_err()
                        {
                            self.state.tx_bad = true;
                        }
                    }
                }
                if command & 1 != 0 {
                    if !self.state.tx_bad && !self.state.tx.is_empty() {
                        self.finish_tx(&desc, frames);
                    }
                    self.state.tx.clear();
                    self.state.tx_bad = false;
                }
            }
            if command & 8 != 0 {
                let _ = self
                    .memory
                    .write_slice(&[desc[12] | 1], GuestAddress(address + 12));
                self.delayed(TXDW, false, command & 0x80 != 0);
            }
            head = (head + 1) % count;
            self.put(TDH, head);
            if head == tail {
                self.cause(TXQE);
            }
        }
    }

    fn insert_checksum(packet: &mut [u8], start: usize, offset: usize, end: usize) {
        let end = if end == 0 {
            packet.len()
        } else {
            end.saturating_add(1).min(packet.len())
        };
        if start >= end || offset.saturating_add(2) > packet.len() {
            return;
        }
        let value = checksum(&packet[start..end]);
        packet[offset..offset + 2].copy_from_slice(&value.to_be_bytes());
    }

    fn finish_tx(&mut self, desc: &[u8; 16], frames: &mut Frames) {
        let extended = desc[11] & 0x20 != 0;
        let tso = extended && desc[11] & 4 != 0;
        let mut packet = std::mem::take(&mut self.state.tx);
        let ctx = if tso {
            self.state.tso_context
        } else {
            self.state.context
        };
        let mut packets = Vec::new();
        if tso {
            let header = usize::from(ctx[13]);
            let mss = usize::from(le16(&ctx, 14));
            let ip = usize::from(ctx[0]);
            let tcp = usize::from(ctx[4]);
            if mss == 0 || header > packet.len() || header < tcp + 20 || ip + 20 > header {
                return;
            }
            let total = packet.len() - header;
            let mut offset = 0;
            let sequence = u32::from_be_bytes(packet[tcp + 4..tcp + 8].try_into().unwrap());
            let ident = u16::from_be_bytes(packet[ip + 4..ip + 6].try_into().unwrap());
            for (index, payload) in packet[header..].chunks(mss).enumerate() {
                let mut segment = packet[..header].to_vec();
                segment.extend_from_slice(payload);
                if ctx[11] & 2 != 0 {
                    let length = (segment.len() - ip) as u16;
                    segment[ip + 2..ip + 4].copy_from_slice(&length.to_be_bytes());
                    segment[ip + 4..ip + 6]
                        .copy_from_slice(&ident.wrapping_add(index as u16).to_be_bytes());
                    segment[ip + 10..ip + 12].fill(0);
                } else if ip + 40 <= header {
                    let length = (segment.len() - ip - 40) as u16;
                    segment[ip + 4..ip + 6].copy_from_slice(&length.to_be_bytes());
                }
                segment[tcp + 4..tcp + 8]
                    .copy_from_slice(&sequence.wrapping_add(offset as u32).to_be_bytes());
                if offset + payload.len() < total {
                    segment[tcp + 13] &= !9;
                }
                if offset != 0 {
                    segment[tcp + 13] &= !0x80;
                }
                let cso = usize::from(ctx[5]);
                if cso + 2 <= segment.len() {
                    let seed = u32::from(u16::from_be_bytes(
                        segment[cso..cso + 2].try_into().unwrap(),
                    ));
                    let mut sum = seed + (segment.len() - tcp) as u32;
                    while sum > 0xffff {
                        sum = (sum & 0xffff) + (sum >> 16);
                    }
                    segment[cso..cso + 2].copy_from_slice(&(sum as u16).to_be_bytes());
                }
                packets.push(segment);
                offset += payload.len();
            }
        } else {
            packets.push(std::mem::take(&mut packet));
        }
        for mut packet in packets {
            if extended {
                if self.state.tx_options & 1 != 0 {
                    Self::insert_checksum(
                        &mut packet,
                        usize::from(ctx[0]),
                        usize::from(ctx[1]),
                        usize::from(le16(&ctx, 2)),
                    );
                }
                if self.state.tx_options & 2 != 0 {
                    Self::insert_checksum(
                        &mut packet,
                        usize::from(ctx[4]),
                        usize::from(ctx[5]),
                        usize::from(le16(&ctx, 6)),
                    );
                }
            } else if desc[11] & 4 != 0 {
                Self::insert_checksum(
                    &mut packet,
                    usize::from(self.state.tx_css),
                    usize::from(desc[10]),
                    0,
                );
            }
            if desc[11] & 0x40 != 0 && self.reg(CTRL) & (1 << 30) != 0 && packet.len() >= 12 {
                let tag = le16(desc, 14).to_be_bytes();
                let mut vlan = (self.reg(0x38) as u16).to_be_bytes().to_vec();
                vlan.extend_from_slice(&tag);
                packet.splice(12..12, vlan);
            }
            if self.reg(TCTL) & 8 != 0 && packet.len() < 60 {
                packet.resize(60, 0);
            }
            let length = packet.len() as u32;
            if self.reg(RCTL) & 0xc0 != 0 {
                frames.receive(packet);
            } else if !frames.transmit(packet) {
                self.cause(TXQE);
                break;
            }
            self.put(0x4080, self.reg(0x4080).wrapping_add(1));
            self.put(0x4090, self.reg(0x4090).wrapping_add(length + 4));
        }
    }

    fn accepts(&self, packet: &[u8]) -> bool {
        if packet.len() < 14 {
            return false;
        }
        let rctl = self.reg(RCTL);
        if packet[..6] == [0xff; 6] {
            return rctl & (1 << 15) != 0;
        }
        let multicast = packet[0] & 1 != 0;
        if rctl & if multicast { 1 << 4 } else { 1 << 3 } != 0 {
            return true;
        }
        for index in 0..16 {
            let high = self.reg(0x5404 + index * 8);
            if high & (1 << 31) != 0
                && packet[..4] == self.reg(0x5400 + index * 8).to_le_bytes()
                && packet[4..6] == (high as u16).to_le_bytes()
            {
                return true;
            }
        }
        if multicast {
            let shift = [4, 3, 2, 0][((rctl >> 12) & 3) as usize];
            let hash = (u16::from_le_bytes([packet[4], packet[5]]) >> shift) & 0xfff;
            return self.reg(0x5200 + usize::from(hash / 32) * 4) & (1 << (hash % 32)) != 0;
        }
        false
    }

    fn rx_checksum(&self, packet: &[u8]) -> (u8, u8, u16) {
        let config = self.reg(0x5000);
        let mut status = 4;
        let mut error = 0;
        let sum = checksum(&packet[(config as usize & 0xff).min(packet.len())..]);
        let ip = if packet.get(12..14) == Some(&[0x81, 0]) {
            18
        } else {
            14
        };
        if packet.len() < ip + 20 || packet[ip] >> 4 != 4 {
            return (status, error, sum);
        }
        let header = usize::from(packet[ip] & 15) * 4;
        let length = usize::from(u16::from_be_bytes([packet[ip + 2], packet[ip + 3]]));
        if header < 20 || header > length || ip + length > packet.len() {
            return (status, error, sum);
        }
        if config & (1 << 8) != 0 {
            status = 0x40;
            if checksum(&packet[ip..ip + header]) != 0 {
                error |= 0x40;
            }
        }
        if config & (1 << 9) != 0
            && packet[ip + 6] & 0x3f == 0
            && packet[ip + 7] == 0
            && matches!(packet[ip + 9], 6 | 17)
        {
            let transport = ip + header;
            if ip + length >= transport + 8 {
                let udp_zero =
                    packet[ip + 9] == 17 && packet[transport + 6..transport + 8] == [0, 0];
                if !udp_zero {
                    status = (status & !4) | 0x20;
                    if packet[ip + 9] == 17 {
                        status |= 0x10;
                    }
                    let mut pseudo = packet[ip + 12..ip + 20].to_vec();
                    pseudo.extend_from_slice(&[0, packet[ip + 9]]);
                    pseudo.extend_from_slice(&((length - header) as u16).to_be_bytes());
                    pseudo.extend_from_slice(&packet[transport..ip + length]);
                    if checksum(&pseudo) != 0 {
                        error |= 0x20;
                    }
                }
            }
        }
        (status, error, sum)
    }

    fn rx(&mut self, frames: &mut Frames) {
        if self.reg(RCTL) & 2 == 0 || !self.state.config.dma() {
            frames.rx.clear();
            return;
        }
        for _ in 0..64 {
            let Some(mut packet) = frames.rx.pop_front() else {
                break;
            };
            if !self.accepts(&packet) {
                continue;
            }
            if packet.len()
                > if self.reg(RCTL) & (1 << 5) != 0 {
                    16384
                } else {
                    1518
                }
            {
                continue;
            }
            let mut vlan = 0;
            let mut vlan_status = 0;
            if packet[12..14] == (self.reg(0x38) as u16).to_be_bytes() && packet.len() >= 18 {
                vlan = u16::from_be_bytes([packet[14], packet[15]]);
                let vid = usize::from(vlan & 0xfff);
                if self.reg(RCTL) & (1 << 18) != 0
                    && self.reg(0x5600 + vid / 32 * 4) & (1 << (vid % 32)) == 0
                {
                    continue;
                }
                if self.reg(CTRL) & (1 << 30) != 0 {
                    packet.drain(12..16);
                    vlan_status = 8;
                }
            }
            let (checksum_status, errors, sum) = self.rx_checksum(&packet);
            let size = packet.len();
            packet.resize(size.max(60), 0);
            if self.reg(RCTL) & (1 << 26) == 0 {
                packet.extend_from_slice(&crc32(&packet).to_le_bytes());
            }
            let Some((base, count, mut head, tail)) = self.ring(RDBAL, RDBAH, RDLEN, RDH, RDT)
            else {
                self.cause(RXO);
                continue;
            };
            let bsize = ((self.reg(RCTL) >> 16) & 3) as usize;
            let capacity = if self.reg(RCTL) & (1 << 25) != 0 {
                [16384, 16384, 8192, 4096][bsize]
            } else {
                [2048, 1024, 512, 256][bsize]
            };
            let required = packet.len().div_ceil(capacity);
            let available = (tail + count - head) % count;
            if required > available as usize {
                self.cause(RXO);
                continue;
            }
            let mut descriptors = Vec::new();
            for index in 0..required {
                let Some(address) = base.checked_add(u64::from((head + index as u32) % count) * 16)
                else {
                    break;
                };
                let Some(desc) = read::<16>(&self.memory, address) else {
                    break;
                };
                let length = (packet.len() - index * capacity).min(capacity);
                if !writable(&self.memory, address + 8, 8)
                    || !writable(&self.memory, le64(&desc, 0), length)
                {
                    break;
                }
                descriptors.push((address, desc, length));
            }
            if descriptors.len() != required {
                self.dma_error(true);
                continue;
            }
            let mut offset = 0;
            for (index, (address, mut desc, length)) in descriptors.into_iter().enumerate() {
                let _ = self.memory.write_slice(
                    &packet[offset..offset + length],
                    GuestAddress(le64(&desc, 0)),
                );
                offset += length;
                desc[8..10].copy_from_slice(&(length as u16).to_le_bytes());
                desc[10..12].copy_from_slice(&sum.to_le_bytes());
                desc[12] = 1 | if index + 1 == required {
                    2 | checksum_status | vlan_status
                } else {
                    0
                };
                desc[13] = errors;
                desc[14..16].copy_from_slice(&vlan.to_le_bytes());
                let _ = self
                    .memory
                    .write_slice(&desc[8..], GuestAddress(address + 8));
                head = (head + 1) % count;
            }
            self.put(RDH, head);
            self.put(0x4074, self.reg(0x4074).wrapping_add(1));
            self.put(0x4088, self.reg(0x4088).wrapping_add(size as u32 + 4));
            let threshold = count >> (1 + ((self.reg(RCTL) >> 8) & 3));
            if (tail + count - head) % count <= threshold {
                self.cause(RXDMT0);
            }
            self.delayed(RXT0, true, true);
        }
    }
}

impl Controller for E1000 {
    fn service(&mut self, elapsed_ns: u64, frames: &mut Frames) {
        self.tick(elapsed_ns);
        self.tx(frames);
        self.rx(frames);
    }
    fn set_link(&mut self, up: bool) {
        let up = up && self.state.phy[0] & 0x0800 == 0;
        if self.state.link == up {
            return;
        }
        self.state.link = up;
        self.put(STATUS, (self.reg(STATUS) & !2) | (u32::from(up) * 2));
        self.state.phy[1] = (self.state.phy[1] & !0x24) | if up { 0x24 } else { 0 };
        self.state.phy[17] = (self.state.phy[17] & !0x2c00) | if up { 0x2c00 } else { 0 };
        self.cause(LSC);
    }
}

impl PciDevice for E1000 {
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
                size: 0x20000,
                kind: BarKind::Mmio32,
            },
            BarInfo {
                index: 1,
                size: 0x40,
                kind: BarKind::Io,
            },
        ]
    }
    fn bar_read(&mut self, bar: u8, offset: u64, data: &mut [u8]) {
        data.fill(0);
        if data.len() != 4 || offset & 3 != 0 {
            return;
        }
        let value = match (bar, offset) {
            (0, 0..0x20000) => self.read_reg(offset as usize),
            (1, 0) => self.state.io_address,
            (1, 4) if self.state.io_address < 0x20000 => {
                self.read_reg((self.state.io_address & !3) as usize)
            }
            _ => 0,
        };
        data.copy_from_slice(&value.to_le_bytes());
    }
    fn bar_write(&mut self, bar: u8, offset: u64, data: &[u8]) {
        if data.len() != 4 || offset & 3 != 0 {
            return;
        }
        let value = le32(data, 0);
        match (bar, offset) {
            (0, 0..0x20000) => self.write_reg(offset as usize, value),
            (1, 0) => self.state.io_address = value,
            (1, 4) if self.state.io_address < 0x20000 => {
                self.write_reg((self.state.io_address & !3) as usize, value)
            }
            _ => {}
        }
    }
}

impl Resettable for E1000 {
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

impl Snapshot for E1000 {
    fn state_id(&self) -> String {
        self.id.clone()
    }
    fn save_state(&self) -> Result<Vec<u8>, DeviceError> {
        serde_json::to_vec(&self.state).map_err(|e| DeviceError::State(e.to_string()))
    }
    fn restore_state(&mut self, state: &[u8]) -> Result<(), DeviceError> {
        let saved: State =
            serde_json::from_slice(state).map_err(|e| DeviceError::State(e.to_string()))?;
        if saved.version != 1
            || saved.config.bytes.len() != 256
            || saved.regs.len() != 0x8000
            || saved.phy.len() != 32
            || saved.eeprom.len() != 64
            || saved.tx.len() > MAX_FRAME
            || saved.serial.bits > 9
            || saved.serial.remaining > 16
        {
            return Err(DeviceError::State("invalid e1000 state".into()));
        }
        self.state = saved;
        self.irq
            .set_level(self.state.irq && self.state.config.intx());
        Ok(())
    }
}

#[cfg(test)]
mod tests;
