//! The PCI root bus: configuration-space routing and BAR placement.
//!
//! The root owns bus 0 with up to 32 slots. Devices are registered with the
//! [`PciDevice`] trait object from `vmlab-hv-devices` and get consecutive
//! slots starting at 1. The root pre-assigns every BAR an address from one of
//! three windows (32-bit MMIO, 64-bit MMIO, port I/O) and reports it on read;
//! when the guest driver sizes and programs the BAR itself, the programmed
//! address takes over and BAR traffic dispatches there.
//!
//! Configuration space is reachable two ways, as on a real PC: the CF8/CFC
//! port pair via [`PciConfigIo`] and the ECAM (MMCONFIG) window via
//! [`PciEcam`]. BAR register dwords (config offsets 16..40) are owned by the
//! root, never forwarded to the device; everything else is forwarded.
//!
//! PCI BAR traffic does not go through [`crate::bus`]; the VMM dispatches
//! accesses inside the BAR windows with [`PciRoot::dispatch_mmio`] and
//! [`PciRoot::dispatch_io`] directly, so one lock (the root) covers config
//! and BAR state.

use parking_lot::{Mutex, MutexGuard};
use std::sync::Arc;

use vmlab_hv_devices::{BarInfo, BarKind, BusDevice, PciDevice};

use crate::layout::{IO_ALLOC_BASE, MMCONFIG_SIZE, MMIO32_BASE, MMIO32_SIZE, MMIO64_SIZE};

/// Number of slots on bus 0.
pub const PCI_SLOTS: usize = 32;

/// Config-space offsets owned by the root: the six BAR dwords.
const BAR_REG_BASE: usize = 4; // dword index of BAR0
const BAR_REG_END: usize = 10; // exclusive

/// One registered PCI function.
struct Function {
    dev: Arc<Mutex<dyn PciDevice>>,
    bars: Vec<BarState>,
    rom: Option<OptionRom>,
}

struct OptionRom {
    bytes: Vec<u8>,
    value: u32,
    sizing: bool,
}

impl OptionRom {
    fn read_config(&self) -> u32 {
        if self.sizing {
            !((self.bytes.len() - 1) as u32) & 0xffff_f800
        } else {
            self.value
        }
    }
}

/// Root-side state of one BAR.
struct BarState {
    prefetchable: bool,
    info: BarInfo,
    /// First config dword of this BAR.
    dword: usize,
    /// Current address: pre-assigned, or guest-programmed.
    addr: u64,
    /// The guest is sizing this BAR (wrote all-ones).
    sizing: bool,
}

impl BarState {
    /// The low-dword flag bits hardwired for this BAR kind.
    fn flags(&self) -> u32 {
        match self.info.kind {
            BarKind::Io => 0x1,
            BarKind::Mmio32 => {
                if self.prefetchable {
                    8
                } else {
                    0
                }
            }
            BarKind::Mmio64 { prefetchable } => 0x4 | if prefetchable { 0x8 } else { 0 },
        }
    }

    fn is_64bit(&self) -> bool {
        matches!(self.info.kind, BarKind::Mmio64 { .. })
    }
}

/// Window allocator for BAR addresses.
#[derive(Default)]
struct BarAllocator {
    io_next: u16,
    mmio32_next: u64,
    mmio64_next: Option<u64>,
}

impl BarAllocator {
    fn new(mmio64_base: u64) -> Self {
        BarAllocator {
            io_next: IO_ALLOC_BASE,
            mmio32_next: MMIO32_BASE,
            mmio64_next: Some(mmio64_base),
        }
    }

    fn allocate(&mut self, kind: BarKind, size: u64) -> Result<u64, String> {
        if !size.is_power_of_two() || size < 16 {
            return Err(format!("BAR size {size:#x} is not a power of two >= 16"));
        }
        match kind {
            BarKind::Io => {
                let (base, len) = (self.io_next as u64, 0x1_0000u64 - self.io_next as u64);
                if size > len {
                    return Err("port I/O BAR window exhausted".into());
                }
                self.io_next += size as u16;
                Ok(base)
            }
            BarKind::Mmio32 => {
                let end = MMIO32_BASE + MMIO32_SIZE;
                if self.mmio32_next + size > end {
                    return Err("32-bit MMIO BAR window exhausted".into());
                }
                let addr = self.mmio32_next;
                self.mmio32_next += size;
                Ok(addr)
            }
            BarKind::Mmio64 { .. } => {
                let base = self.mmio64_base();
                let end = base + MMIO64_SIZE;
                let next = self.mmio64_next.unwrap();
                if next + size > end {
                    return Err("64-bit MMIO BAR window exhausted".into());
                }
                self.mmio64_next = Some(next + size);
                Ok(next)
            }
        }
    }

    fn mmio64_base(&self) -> u64 {
        self.mmio64_next.unwrap_or(0)
    }
}

/// The PCI root of the machine. Shared behind an `Arc<Mutex<...>>`.
pub struct PciRoot {
    slots: Vec<Option<Function>>,
    alloc: BarAllocator,
    firmware: bool,
}

impl PciRoot {
    pub fn addresses(&self) -> Vec<(u8, u8)> {
        self.slots
            .iter()
            .enumerate()
            .filter_map(|(i, f)| f.as_ref().map(|_| ((i / 8) as u8, (i % 8) as u8)))
            .collect()
    }
    /// An empty root with the 64-bit BAR window at `mmio64_base`.
    pub fn new(mmio64_base: u64) -> Self {
        PciRoot {
            slots: (0..PCI_SLOTS * 8).map(|_| None).collect(),
            alloc: BarAllocator::new(mmio64_base),
            firmware: false,
        }
    }

    /// Q35 leaves BAR addresses and command decoding disabled until firmware
    /// programs them. Functions are placed with `add_device_at`.
    pub fn q35(mmio64_base: u64) -> Self {
        Self {
            firmware: true,
            ..Self::new(mmio64_base)
        }
    }

    /// Place a device on the next free slot (1..31); returns the slot.
    pub fn add_device(&mut self, dev: Arc<Mutex<dyn PciDevice>>) -> Result<u8, String> {
        let slot = self
            .slots
            .iter()
            .enumerate()
            .skip(8)
            .find(|(i, s)| i % 8 == 0 && s.is_none())
            .map(|(i, _)| i)
            .ok_or("no free PCI slot")?;
        self.add_device_at((slot / 8) as u8, 0, dev)?;
        Ok((slot / 8) as u8)
    }

    /// Place a function at a specific bus-zero BDF. No implicit relocation.
    pub fn add_device_at(
        &mut self,
        slot: u8,
        function: u8,
        dev: Arc<Mutex<dyn PciDevice>>,
    ) -> Result<(), String> {
        if slot >= 32 || function >= 8 {
            return Err("PCI device/function out of range".into());
        }
        let index = slot as usize * 8 + function as usize;
        if self.slots[index].is_some() {
            return Err(format!("PCI address 00:{slot:02x}.{function} is occupied"));
        }
        let infos = {
            let d = dev.lock();
            d.bars()
        };
        let mut bars = Vec::new();
        let mut rom = None;
        for info in infos {
            if info.index == 6 {
                if !info.size.is_power_of_two() || info.size < 2048 || info.size > 16 << 20 {
                    return Err("invalid expansion ROM size".into());
                }
                let mut bytes = vec![0xff; info.size as usize];
                dev.lock().bar_read(6, 0, &mut bytes);
                rom = Some(OptionRom {
                    bytes,
                    value: 0,
                    sizing: false,
                });
                continue;
            }
            // BAR `i` lives at config dword 4+i; a 64-bit BAR also owns 4+i+1.
            let dword = BAR_REG_BASE + info.index as usize;
            let span = if matches!(info.kind, BarKind::Mmio64 { .. }) {
                2
            } else {
                1
            };
            if dword + span > BAR_REG_END {
                return Err(format!("BAR {} does not fit in config space", info.index));
            }
            if bars.iter().any(|b: &BarState| {
                (b.dword..b.dword + if b.is_64bit() { 2 } else { 1 })
                    .any(|d| (dword..dword + span).contains(&d))
            }) {
                return Err(format!("BAR {} overlaps another BAR", info.index));
            }
            if !info.size.is_power_of_two() || info.size < 4 {
                return Err(format!("invalid BAR size {}", info.size));
            }
            let addr = if self.firmware {
                0
            } else {
                self.alloc.allocate(info.kind, info.size)?
            };
            bars.push(BarState {
                prefetchable: matches!(info.kind, BarKind::Mmio32)
                    && dev.lock().config_read(dword) & 8 != 0,
                info,
                dword,
                addr,
                sizing: false,
            });
        }
        self.slots[index] = Some(Function { dev, bars, rom });
        Ok(())
    }

    /// Attach a PCI expansion ROM. Firmware discovers, sizes and enables it
    /// through the type-zero ROM BAR at 0x30.
    pub fn attach_option_rom(
        &mut self,
        slot: u8,
        function: u8,
        image: &[u8],
    ) -> Result<(), String> {
        if image.len() < 3 || image[..2] != [0x55, 0xaa] || image.len() > 16 * 1024 * 1024 {
            return Err("invalid PCI option ROM signature or size".into());
        }
        let func = self
            .slots
            .get_mut(slot as usize * 8 + function as usize)
            .filter(|_| slot < 32 && function < 8)
            .and_then(Option::as_mut)
            .ok_or("option ROM names an absent PCI function")?;
        if func.rom.is_some() {
            return Err("PCI function already has an option ROM".into());
        }
        let mut bytes = vec![0xff; image.len().max(2048).next_power_of_two()];
        bytes[..image.len()].copy_from_slice(image);
        func.rom = Some(OptionRom {
            bytes,
            value: 0,
            sizing: false,
        });
        Ok(())
    }

    /// Read config dword `reg` of `(bus, slot, function)`.
    pub fn config_read(&mut self, bus: u8, slot: u8, function: u8, reg: usize) -> u32 {
        if bus != 0 || function >= 8 || slot as usize >= PCI_SLOTS || reg >= 1024 {
            return 0xFFFF_FFFF;
        }
        let Some(func) = self.slots[slot as usize * 8 + function as usize].as_ref() else {
            return 0xFFFF_FFFF;
        };
        if reg == 12
            && let Some(rom) = &func.rom
        {
            return rom.read_config();
        }
        if let Some(bar) = func.bars.iter().find(|b| b.dword == reg) {
            return bar_low_read(bar);
        }
        if let Some(bar) = func
            .bars
            .iter()
            .find(|b| b.is_64bit() && b.dword + 1 == reg)
        {
            return bar_high_read(bar);
        }
        let mut dev = func.dev.lock();
        let mut value = dev.config_read(reg);
        if reg == 3
            && function == 0
            && self.slots[slot as usize * 8 + 1..slot as usize * 8 + 8]
                .iter()
                .any(Option::is_some)
        {
            value |= 0x0080_0000;
        }
        value
    }

    /// Write config bytes at `offset` (0..256) of `(bus, slot, function)`.
    pub fn config_write(&mut self, bus: u8, slot: u8, function: u8, offset: usize, data: &[u8]) {
        if bus != 0
            || function >= 8
            || slot as usize >= PCI_SLOTS
            || offset >= 4096
            || data.len() > 4096 - offset
        {
            return;
        }
        let Some(func) = self.slots[slot as usize * 8 + function as usize].as_mut() else {
            return;
        };
        // Split at dword boundaries so the device API (one dword per call)
        // is honoured even for accesses that straddle.
        let mut offset = offset;
        let mut data = data;
        while !data.is_empty() {
            let reg = offset / 4;
            let within = offset % 4;
            let n = data.len().min(4 - within);
            let chunk = &data[..n];
            if reg == 12
                && let Some(rom) = &mut func.rom
            {
                let mut value = rom.read_config().to_le_bytes();
                value[within..within + n].copy_from_slice(chunk);
                let value = u32::from_le_bytes(value);
                rom.sizing = value & 0xffff_f800 == 0xffff_f800;
                if !rom.sizing {
                    rom.value = value & (!((rom.bytes.len() - 1) as u32) | 1);
                }
                offset += n;
                data = &data[n..];
                continue;
            }
            let is_low = |reg: usize| func.bars.iter().any(|b| b.dword == reg);
            let is_high = |reg: usize| func.bars.iter().any(|b| b.is_64bit() && b.dword + 1 == reg);
            if is_low(reg) {
                let idx = func.bars.iter().position(|b| b.dword == reg).unwrap();
                Self::write_low_dword(&mut func.bars[idx], within, chunk);
            } else if is_high(reg) {
                let idx = func
                    .bars
                    .iter()
                    .position(|b| b.is_64bit() && b.dword + 1 == reg)
                    .unwrap();
                Self::write_high_dword(&mut func.bars[idx], within, chunk);
            } else if reg < 1024 && !(self.firmware && (BAR_REG_BASE..BAR_REG_END).contains(&reg)) {
                let mut dev = func.dev.lock();
                dev.config_write(reg, within as u64, chunk);
            }
            offset += n;
            data = &data[n..];
        }
        let mut dev = func.dev.lock();
        for bar in &func.bars {
            dev.set_bar_address(
                bar.info.index,
                (bar.addr != 0 && !bar.sizing).then_some(bar.addr),
            );
        }
    }

    pub(crate) fn take_error(&mut self) -> Option<String> {
        self.slots
            .iter_mut()
            .flatten()
            .find_map(|f| f.dev.lock().take_error())
    }

    /// Overlay a partial write on the BAR's low dword and interpret it. The
    /// high dword of a 64-bit BAR is preserved: drivers may program the two
    /// dwords in either order.
    fn write_low_dword(bar: &mut BarState, within: usize, chunk: &[u8]) {
        let mut current = bar_low_read(bar).to_le_bytes();
        current[within..within + chunk.len()].copy_from_slice(chunk);
        let value = u32::from_le_bytes(current);
        let addr_bits_mask = match bar.info.kind {
            BarKind::Io => 0xFFFF_FFFCu32,
            _ => 0xFFFF_FFF0u32,
        };
        if value & addr_bits_mask == addr_bits_mask {
            bar.sizing = true;
        } else {
            bar.sizing = false;
            let high = if bar.is_64bit() {
                bar.addr & 0xFFFF_FFFF_0000_0000
            } else {
                0
            };
            bar.addr = (high | u64::from(value & addr_bits_mask)) & !(bar.info.size - 1);
        }
    }

    fn write_high_dword(bar: &mut BarState, within: usize, chunk: &[u8]) {
        let mut current = bar_high_read(bar).to_le_bytes();
        current[within..within + chunk.len()].copy_from_slice(chunk);
        let value = u32::from_le_bytes(current);
        if value == 0xFFFF_FFFF {
            // Stays in sizing until the low dword gets a real address; keep
            // the high half's all-ones read without a separate flag.
            bar.addr &= 0xFFFF_FFFF; // low half unchanged
            bar.sizing = true;
        } else {
            bar.sizing = false;
            bar.addr = (bar.addr & 0xFFFF_FFFF) | (u64::from(value) << 32);
        }
    }

    /// Dispatch an MMIO read inside a BAR window. Returns whether a BAR
    /// covered it.
    pub fn dispatch_mmio_read(&mut self, addr: u64, data: &mut [u8]) -> bool {
        if let Some((rom, offset)) = self.find_rom(addr) {
            for (i, byte) in data.iter_mut().enumerate() {
                *byte = rom.get(offset + i).copied().unwrap_or(0xff);
            }
            return true;
        }
        self.find_bar(addr, false)
            .map(|(mut dev, bar, offset)| dev.bar_read(bar, offset, data))
            .is_some()
    }

    /// Dispatch an MMIO write inside a BAR window.
    pub fn dispatch_mmio_write(&mut self, addr: u64, data: &[u8]) -> bool {
        if self.find_rom(addr).is_some() {
            return true;
        }
        self.find_bar(addr, false)
            .map(|(mut dev, bar, offset)| dev.bar_write(bar, offset, data))
            .is_some()
    }

    fn find_rom(&self, addr: u64) -> Option<(&[u8], usize)> {
        for func in self.slots.iter().flatten() {
            let Some(rom) = &func.rom else { continue };
            let base = (rom.value & !0x7ff) as u64;
            if rom.value & 1 != 0
                && !rom.sizing
                && func.dev.lock().config_read(1) & 2 != 0
                && addr >= base
                && addr - base < rom.bytes.len() as u64
            {
                return Some((&rom.bytes, (addr - base) as usize));
            }
        }
        None
    }

    /// Dispatch a port-I/O read inside a BAR window.
    pub fn dispatch_io_read(&mut self, port: u16, data: &mut [u8]) -> bool {
        self.find_bar(u64::from(port), true)
            .map(|(mut dev, bar, offset)| dev.bar_read(bar, offset, data))
            .is_some()
    }

    /// Dispatch a port-I/O write inside a BAR window.
    pub fn dispatch_io_write(&mut self, port: u16, data: &[u8]) -> bool {
        self.find_bar(u64::from(port), true)
            .map(|(mut dev, bar, offset)| dev.bar_write(bar, offset, data))
            .is_some()
    }

    /// Find the function and BAR covering `addr`, locking its device.
    fn find_bar(
        &mut self,
        addr: u64,
        io: bool,
    ) -> Option<(MutexGuard<'_, dyn PciDevice>, u8, u64)> {
        for func in self.slots.iter_mut().filter_map(|s| s.as_mut()) {
            if self.firmware && func.dev.lock().config_read(1) & if io { 1 } else { 2 } == 0 {
                continue;
            }
            for bar in &func.bars {
                if matches!(bar.info.kind, BarKind::Io) == io
                    && !bar.sizing
                    && bar.addr != 0
                    && bar.addr <= addr
                    && addr - bar.addr < bar.info.size
                {
                    let offset = addr - bar.addr;
                    let dev = func.dev.lock();
                    return Some((dev, bar.info.index, offset));
                }
            }
        }
        None
    }

    /// Root-side BAR state for snapshots: per occupied slot, the programmed
    /// address and mid-sizing flag of each BAR, in registration order. The
    /// pre-assigned addresses are deterministic for a machine description,
    /// so only the guest-driven part needs saving.
    pub fn save_bar_states(&self) -> Vec<(u8, Vec<(u64, bool)>)> {
        self.slots
            .iter()
            .enumerate()
            .filter_map(|(slot, func)| {
                func.as_ref().map(|f| {
                    (
                        if self.firmware {
                            slot as u8
                        } else {
                            (slot / 8) as u8
                        },
                        f.bars
                            .iter()
                            .map(|b| (b.addr, b.sizing))
                            .chain(f.rom.iter().map(|rom| (rom.value as u64, rom.sizing)))
                            .collect(),
                    )
                })
            })
            .collect()
    }

    /// Restore BAR state saved by [`PciRoot::save_bar_states`]. The slot
    /// and BAR layout must match, which the machine-description digest
    /// guarantees for a compatible snapshot.
    pub fn restore_bar_states(&mut self, saved: &[(u8, Vec<(u64, bool)>)]) -> Result<(), String> {
        for (slot, bars) in saved {
            let index = if self.firmware {
                *slot as usize
            } else {
                *slot as usize * 8
            };
            let Some(func) = self.slots.get_mut(index).and_then(|s| s.as_mut()) else {
                return Err(format!("saved PCI state names slot {slot}, which is empty"));
            };
            if func.bars.len() + usize::from(func.rom.is_some()) != bars.len() {
                return Err(format!(
                    "saved PCI state for slot {slot} has {} bars, this machine has {}",
                    bars.len(),
                    func.bars.len()
                ));
            }
            for (bar, (addr, sizing)) in func.bars.iter_mut().zip(bars) {
                bar.addr = *addr;
                bar.sizing = *sizing;
            }
            if let Some(rom) = &mut func.rom {
                let (value, sizing) = bars.last().unwrap();
                rom.value = *value as u32;
                rom.sizing = *sizing;
            }
        }
        Ok(())
    }
}

fn bar_low_read(bar: &BarState) -> u32 {
    if bar.sizing {
        let mask = !(bar.info.size - 1) as u32;
        let addr_bits = match bar.info.kind {
            BarKind::Io => mask & 0xFFFF_FFFC,
            _ => mask & 0xFFFF_FFF0,
        };
        addr_bits | bar.flags()
    } else {
        (bar.addr as u32) | bar.flags()
    }
}

fn bar_high_read(bar: &BarState) -> u32 {
    if bar.sizing {
        (!(bar.info.size - 1) >> 32) as u32
    } else {
        (bar.addr >> 32) as u32
    }
}

/// The CF8/CFC configuration port pair, registered on the port-I/O bus at
/// 0xCF8 (covering CF8..D00).
pub struct PciConfigIo {
    root: Arc<Mutex<PciRoot>>,
    address: u32,
}

impl PciConfigIo {
    pub fn new(root: Arc<Mutex<PciRoot>>) -> Self {
        PciConfigIo { root, address: 0 }
    }

    fn bdf(&self) -> (u8, u8, u8, usize) {
        let a = self.address;
        let bus = ((a >> 16) & 0xFF) as u8;
        let slot = ((a >> 11) & 0x1F) as u8;
        let function = ((a >> 8) & 0x7) as u8;
        let reg = ((a >> 2) & 0x3F) as usize;
        (bus, slot, function, reg)
    }
}

impl BusDevice for PciConfigIo {
    fn read(&mut self, offset: u64, data: &mut [u8]) {
        match offset {
            0..=3 => {
                let bytes = self.address.to_le_bytes();
                copy_into(data, &bytes[offset as usize..]);
            }
            4..=7 if self.address & 0x8000_0000 != 0 => {
                let (bus, slot, function, reg) = self.bdf();
                let dword = self.root.lock().config_read(bus, slot, function, reg);
                let bytes = dword.to_le_bytes();
                copy_into(data, &bytes[(offset - 4) as usize..]);
            }
            _ => data.fill(0xFF),
        }
    }

    fn write(&mut self, offset: u64, data: &[u8]) {
        match offset {
            0..=3 => {
                let mut bytes = self.address.to_le_bytes();
                overlay(&mut bytes[offset as usize..], data);
                self.address = u32::from_le_bytes(bytes);
            }
            4..=7 if self.address & 0x8000_0000 != 0 => {
                let (bus, slot, function, reg) = self.bdf();
                let offset = reg * 4 + (offset - 4) as usize;
                self.root
                    .lock()
                    .config_write(bus, slot, function, offset, data);
            }
            _ => {}
        }
    }
}

/// The ECAM (MMCONFIG) window: 256 MiB at [`MMCONFIG_BASE`], encoding
/// `(bus << 20) | (slot << 15) | (function << 12) | register offset`.
pub struct PciEcam {
    root: Arc<Mutex<PciRoot>>,
}

impl PciEcam {
    pub fn new(root: Arc<Mutex<PciRoot>>) -> Self {
        PciEcam { root }
    }
}

impl BusDevice for PciEcam {
    fn read(&mut self, offset: u64, data: &mut [u8]) {
        if offset >= MMCONFIG_SIZE {
            data.fill(0xFF);
            return;
        }
        let mut root = self.root.lock();
        for (index, byte) in data.iter_mut().enumerate() {
            let address = offset.saturating_add(index as u64);
            if address >= MMCONFIG_SIZE {
                *byte = 0xff;
                continue;
            }
            let (bus, slot, function, conf_off) = ecam_decode(address);
            *byte = root
                .config_read(bus, slot, function, conf_off / 4)
                .to_le_bytes()[conf_off % 4];
        }
    }

    fn write(&mut self, offset: u64, data: &[u8]) {
        if offset >= MMCONFIG_SIZE {
            return;
        }
        let (bus, slot, function, conf_off) = ecam_decode(offset);
        self.root
            .lock()
            .config_write(bus, slot, function, conf_off, data);
    }
}

fn ecam_decode(offset: u64) -> (u8, u8, u8, usize) {
    let bus = (offset >> 20) as u8;
    let slot = ((offset >> 15) & 0x1F) as u8;
    let function = ((offset >> 12) & 0x7) as u8;
    let conf_off = (offset & 0xFFF) as usize;
    (bus, slot, function, conf_off)
}

fn copy_into(data: &mut [u8], source: &[u8]) {
    let n = data.len().min(source.len());
    data[..n].copy_from_slice(&source[..n]);
    data[n..].fill(0xFF);
}

fn overlay(target: &mut [u8], source: &[u8]) {
    let n = target.len().min(source.len());
    target[..n].copy_from_slice(&source[..n]);
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One recorded BAR or config access: register/index, offset, bytes,
    /// write flag.
    type Access = (u8, u64, Vec<u8>, bool);

    /// A minimal PCI function: a vendor dword, two BARs, and a log of BAR
    /// and config traffic.
    struct FakePci {
        bars: Vec<BarInfo>,
        bar_log: Mutex<Vec<Access>>,
        config_log: Mutex<Vec<(usize, u64, Vec<u8>)>>,
    }

    impl FakePci {
        fn new(bars: Vec<BarInfo>) -> Arc<Mutex<FakePci>> {
            Arc::new(Mutex::new(FakePci {
                bars,
                bar_log: Mutex::new(Vec::new()),
                config_log: Mutex::new(Vec::new()),
            }))
        }
    }

    impl PciDevice for FakePci {
        fn config_read(&mut self, reg: usize) -> u32 {
            match reg {
                0 => 0xABCD_1234,
                2 => 0x1000_0000, // class code in the top byte
                _ => 0,
            }
        }

        fn config_write(&mut self, reg: usize, offset: u64, data: &[u8]) {
            self.config_log.lock().push((reg, offset, data.to_vec()));
        }

        fn bars(&self) -> Vec<BarInfo> {
            self.bars.clone()
        }

        fn bar_read(&mut self, bar: u8, offset: u64, data: &mut [u8]) {
            self.bar_log
                .lock()
                .push((bar, offset, vec![0xAA; data.len()], false));
            data.fill(0xAA);
        }

        fn bar_write(&mut self, bar: u8, offset: u64, data: &[u8]) {
            self.bar_log.lock().push((bar, offset, data.to_vec(), true));
        }
    }

    fn test_bars() -> Vec<BarInfo> {
        vec![
            BarInfo {
                index: 0,
                size: 0x1000,
                kind: BarKind::Mmio32,
            },
            BarInfo {
                index: 2,
                size: 0x2000,
                kind: BarKind::Mmio64 {
                    prefetchable: false,
                },
            },
        ]
    }

    fn root_with_fake() -> (Arc<Mutex<PciRoot>>, Arc<Mutex<FakePci>>) {
        let dev = FakePci::new(test_bars());
        let mut root = PciRoot::new(0x1_0000_0000);
        root.add_device(dev.clone()).unwrap();
        (Arc::new(Mutex::new(root)), dev)
    }

    #[test]
    fn bar_assignment_is_reported_with_flags() {
        let (root, dev) = root_with_fake();
        let mut root = root.lock();
        let mut d = dev.lock();
        // BAR0 at the bottom of the 32-bit window, plain MMIO flags.
        assert_eq!(root.config_read(0, 1, 0, 4), MMIO32_BASE as u32);
        // BAR2 (64-bit) at the bottom of the 64-bit window (4 GiB): the low
        // dword is 0 | the 64-bit type flag, the high dword is 1.
        assert_eq!(root.config_read(0, 1, 0, 6), 0x4);
        assert_eq!(root.config_read(0, 1, 0, 7), 1);
        assert_eq!(d.config_read(0), 0xABCD_1234);
    }

    #[test]
    fn bar_sizing_round_trip() {
        let (root, _dev) = root_with_fake();
        let mut root = root.lock();
        // Driver sizes BAR0: write all ones, read back size mask | flags.
        root.config_write(0, 1, 0, 16, &0xFFFF_FFF0u32.to_le_bytes());
        assert_eq!(root.config_read(0, 1, 0, 4), 0xFFFF_F000);
        // Driver assigns an address inside the window.
        root.config_write(0, 1, 0, 16, &0xC100_0000u32.to_le_bytes());
        assert_eq!(root.config_read(0, 1, 0, 4), 0xC100_0000);
        // 64-bit BAR: size both dwords, then program a >4G address.
        root.config_write(0, 1, 0, 24, &0xFFFF_FFFFu32.to_le_bytes());
        root.config_write(0, 1, 0, 28, &0xFFFF_FFFFu32.to_le_bytes());
        assert_eq!(root.config_read(0, 1, 0, 6), 0xFFFF_E004);
        assert_eq!(root.config_read(0, 1, 0, 7), 0xFFFF_FFFF);
        root.config_write(0, 1, 0, 24, &0x2000_0000u32.to_le_bytes());
        root.config_write(0, 1, 0, 28, &2u32.to_le_bytes());
        assert_eq!(root.config_read(0, 1, 0, 6), 0x2000_0004);
        assert_eq!(root.config_read(0, 1, 0, 7), 2);
    }

    #[test]
    fn bar_dispatch_follows_programmed_address() {
        let (root, dev) = root_with_fake();
        let mut root = root.lock();
        // Not programmed yet: dispatches at the pre-assigned address.
        let mut data = [0u8; 2];
        assert!(root.dispatch_mmio_read(MMIO32_BASE + 0x40, &mut data));
        assert_eq!(data, [0xAA, 0xAA]);
        // Program it elsewhere; dispatch follows.
        root.config_write(0, 1, 0, 16, &0xC200_0000u32.to_le_bytes());
        assert!(root.dispatch_mmio_write(0xC200_0000 + 8, &[0x55, 0x66]));
        let log = dev.lock().bar_log.lock().clone();
        assert_eq!(log.last().unwrap(), &(0, 8, vec![0x55, 0x66], true));
        // Outside any BAR: unhandled.
        assert!(!root.dispatch_mmio_read(0xDEAD_0000, &mut data));
    }

    #[test]
    fn cf8_cfc_routes_config_reads() {
        let (root, _dev) = root_with_fake();
        let root = Arc::try_unwrap(root).ok().unwrap().into_inner();
        let shared = Arc::new(Mutex::new(root));
        let mut cfg = PciConfigIo::new(shared.clone());
        // Address register read-back.
        cfg.write(0, &0x8000_0800u32.to_le_bytes());
        let mut out = [0u8; 4];
        cfg.read(0, &mut out);
        assert_eq!(u32::from_le_bytes(out), 0x8000_0800);
        // Vendor dword of slot 1, reg 0, through CFC.
        cfg.write(0, &0x8000_0800u32.to_le_bytes()); // bus 0, slot 1, fn 0, reg 0
        cfg.read(4, &mut out);
        assert_eq!(u32::from_le_bytes(out), 0xABCD_1234);
        // Empty slot 2 reads all ones.
        cfg.write(0, &0x8000_1000u32.to_le_bytes());
        cfg.read(4, &mut out);
        assert_eq!(u32::from_le_bytes(out), 0xFFFF_FFFF);
        // BAR write through CFC: address register selects dword 4 (BAR0).
        cfg.write(0, &0x8000_0810u32.to_le_bytes());
        cfg.write(4, &0xFFFF_FFF0u32.to_le_bytes());
        assert_eq!(shared.lock().config_read(0, 1, 0, 4), 0xFFFF_F000);
    }

    #[test]
    fn ecam_reads_config_space() {
        let (root, _dev) = root_with_fake();
        let root = Arc::try_unwrap(root).ok().unwrap().into_inner();
        let mut ecam = PciEcam::new(Arc::new(Mutex::new(root)));
        let mut out = [0u8; 4];
        // Slot 1, function 0, register 0.
        ecam.read((1usize << 15) as u64, &mut out);
        assert_eq!(u32::from_le_bytes(out), 0xABCD_1234);
        // Beyond the window reads 0xFF.
        ecam.read(MMCONFIG_SIZE + 4, &mut out);
        assert_eq!(out, [0xFF; 4]);
        // A BAR write via ECAM reaches the root.
        ecam.write((1usize << 15) as u64 + 16, &0xFFFF_FFF0u32.to_le_bytes());
    }

    #[test]
    fn non_bar_writes_reach_the_device() {
        let (root, dev) = root_with_fake();
        root.lock().config_write(0, 1, 0, 4, &[0x07, 0x00]);
        let log = dev.lock().config_log.lock().clone();
        assert_eq!(log, vec![(1, 0, vec![0x07, 0x00])]);
    }

    #[test]
    fn io_bar_allocation_and_dispatch() {
        let dev = FakePci::new(vec![BarInfo {
            index: 3,
            size: 0x100,
            kind: BarKind::Io,
        }]);
        let mut root = PciRoot::new(0x1_0000_0000);
        root.add_device(dev.clone()).unwrap();
        let root_cell = Mutex::new(root);
        let mut root = root_cell.lock();
        assert_eq!(root.config_read(0, 1, 0, 7), u32::from(IO_ALLOC_BASE) | 0x1);
        let data = [0u8; 1];
        assert!(root.dispatch_io_write(IO_ALLOC_BASE + 3, &data));
        assert!(!root.dispatch_io_write(0x3F8, &data));
    }

    #[test]
    fn explicit_functions_extended_config_and_firmware_bars() {
        let mut root = PciRoot::q35(1 << 32);
        let dev = FakePci::new(test_bars());
        root.add_device_at(31, 3, dev.clone()).unwrap();
        root.add_device_at(31, 0, FakePci::new(vec![])).unwrap();
        assert_eq!(root.config_read(0, 31, 0, 3) & 0x0080_0000, 0x0080_0000);
        assert!(root.add_device_at(31, 3, dev.clone()).is_err());
        assert!(root.add_device_at(32, 0, dev.clone()).is_err());
        assert!(root.add_device_at(0, 8, dev.clone()).is_err());
        assert_eq!(root.config_read(0, 31, 3, 4), 0);
        root.config_write(0, 31, 3, 0xffc, &[1, 2, 3, 4]);
        assert_eq!(
            dev.lock().config_log.lock().last().unwrap(),
            &(1023, 0, vec![1, 2, 3, 4])
        );
        assert_eq!(root.config_read(0, 31, 3, 1024), u32::MAX);
        assert_eq!(root.config_read(1, 31, 3, 0), u32::MAX);
        root.config_write(0, 31, 3, 16, &0xc001_ffffu32.to_le_bytes());
        assert_eq!(root.config_read(0, 31, 3, 4), 0xc001_f000);
        let saved = root.save_bar_states();
        root.config_write(0, 31, 3, 16, &0xc002_0000u32.to_le_bytes());
        root.restore_bar_states(&saved).unwrap();
        assert_eq!(root.config_read(0, 31, 3, 4), 0xc001_f000);
    }

    #[test]
    fn cf8_disable_bit_and_cfc_location() {
        let (root, _) = root_with_fake();
        let mut io = PciConfigIo::new(root);
        let mut bytes = [0; 4];
        io.write(0, &0x800u32.to_le_bytes());
        io.read(4, &mut bytes);
        assert_eq!(bytes, [0xff; 4]);
        io.write(0, &0x8000_0800u32.to_le_bytes());
        io.read(4, &mut bytes);
        assert_eq!(u32::from_le_bytes(bytes), 0xabcd_1234);
        io.read(8, &mut bytes);
        assert_eq!(bytes, [0xff; 4]);
    }

    #[test]
    fn native_vga_rom_bar_and_prefetchable_framebuffer() {
        let mut root = PciRoot::q35(1 << 32);
        let vga = Arc::new(Mutex::new(
            vmlab_hv_devices::display::StandardVga::new("vga", &[0x55, 0xaa, 1]).unwrap(),
        ));
        root.add_device_at(4, 0, vga).unwrap();
        assert_eq!(root.config_read(0, 4, 0, 4), 8);
        root.config_write(0, 4, 0, 0x30, &u32::MAX.to_le_bytes());
        assert_eq!(root.config_read(0, 4, 0, 12), 0xfffe_0000);
        root.config_write(0, 4, 0, 0x30, &0xfebc_0001u32.to_le_bytes());
        let mut signature = [0; 3];
        assert!(root.dispatch_mmio_read(0xfebc_0000, &mut signature));
        assert_eq!(signature, [0x55, 0xaa, 1]);
        assert!(root.dispatch_mmio_write(0xfebc_0000, &[0; 3]));
        root.config_write(0, 4, 0, 0x30, &0xfebc_0000u32.to_le_bytes());
        assert!(!root.dispatch_mmio_read(0xfebc_0000, &mut signature));
    }
}
