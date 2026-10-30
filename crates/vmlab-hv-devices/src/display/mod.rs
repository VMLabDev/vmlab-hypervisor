//! Standard VGA and Bochs DISPI, independent of the VMM.
//!
//! Route PCI configuration and BARs through `PciDevice`. Route ports 0x3b0..=0x3df
//! and 0x1ce..=0x1d0 through `io_read`/`io_write` using absolute port numbers.
//! Route 0xa0000..0xc0000 through `legacy_read`/`legacy_write` with offsets
//! relative to 0xa0000. Serialize registers and frame publication with one lock.
//! Direct BAR0 mappings use the volatile storage returned by `linear_framebuffer`.
//! BAR0 is prefetchable, as reported by config space. `BarKind::Mmio32` cannot
//! express that flag. The PCI root must preserve bit 3 when synthesizing BAR0.
//! `ROM_BAR` is an expansion ROM at config offset 0x30, not a normal BAR at
//! 0x10 + index * 4. Route it only while bit 0 of config offset 0x30 is set.
//!
//! Register references: FreeVGA's VGA register and memory specifications,
//! https://www.scs.stanford.edu/10wi-cs140/pintos/specs/freevga/vga/vga.htm,
//! and https://www.qemu.org/docs/master/specs/standard-vga.html, a specification
//! document. PCI identity follows docs/hwref/q35-seabios-linux-generic.

mod font;
#[cfg(test)]
mod tests;
pub mod virtio_vga;

use crate::{BarInfo, BarKind, DeviceError, PciDevice, Snapshot};
use vm_memory::{Bytes, GuestAddress, GuestMemoryMmap};

pub const VRAM_SIZE: usize = 16 * 1024 * 1024;
pub const ROM_SIZE: usize = 128 * 1024;
/// BAR number used by `bar_read` for the PCI expansion ROM.
pub const ROM_BAR: u8 = 6;
const SEQ: usize = 0;
const GC: usize = 256;
const CRTC: usize = 512;
const ATTR: usize = 768;
const DAC: usize = 1024;
const REG_SIZE: usize = DAC + 768;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PixelFormat {
    /// Bytes are blue, green, red, zero. DRM_FORMAT_XRGB8888.
    Xrgb8888,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Mode {
    pub width: u32,
    pub height: u32,
    /// Stride of the converted frame, in bytes.
    pub stride: u32,
    pub format: PixelFormat,
}

pub struct Frame {
    pub mode: Mode,
    pub pixels: Vec<u8>,
    /// Full-frame damage since the previous `frame` call, including mode changes.
    pub dirty: bool,
}

/// A single standard VGA PCI function. Firmware supplies the option ROM bytes.
pub struct StandardVga {
    id: String,
    config: [u8; 256],
    regs: [u8; REG_SIZE],
    dispi: [u16; 16],
    vram: GuestMemoryMmap,
    rom: Vec<u8>,
    seq_index: u8,
    gc_index: u8,
    crtc_index: u8,
    attr_index: u8,
    attr_data: bool,
    misc: u8,
    feature: u8,
    dac_mask: u8,
    dac_read: u8,
    dac_write: u8,
    dac_component: u8,
    dac_state: u8,
    dispi_index: u16,
    latch: [u8; 4],
    status: u8,
    big_endian: bool,
    dirty: bool,
    cached_pixels: Option<Vec<u8>>,
    render_vram: Vec<u8>,
}

impl StandardVga {
    /// `rom` is the host's SeaVGABIOS image, not a QEMU-built option ROM.
    /// Images must have the PCI ROM signature and fit the captured 128 KiB BAR.
    pub fn new(id: impl Into<String>, rom: &[u8]) -> Result<Self, DeviceError> {
        if rom.len() < 3 || rom.len() > ROM_SIZE || rom[..2] != [0x55, 0xaa] {
            return Err(DeviceError::Other("invalid VGA option ROM".into()));
        }
        let mut dev = Self {
            id: id.into(),
            config: [0; 256],
            regs: [0; REG_SIZE],
            dispi: [0; 16],
            vram: GuestMemoryMmap::from_ranges(&[(GuestAddress(0), VRAM_SIZE)])
                .map_err(|e| DeviceError::Memory(e.to_string()))?,
            rom: vec![0xff; ROM_SIZE],
            seq_index: 0,
            gc_index: 0,
            crtc_index: 0,
            attr_index: 0x20,
            attr_data: false,
            misc: 1,
            feature: 0,
            dac_mask: 0xff,
            dac_read: 0,
            dac_write: 0,
            dac_component: 0,
            dac_state: 0,
            dispi_index: 0,
            latch: [0; 4],
            status: 0,
            big_endian: false,
            dirty: true,
            cached_pixels: None,
            render_vram: Vec::new(),
        };
        dev.rom[..rom.len()].copy_from_slice(rom);
        dev.config[..4].copy_from_slice(&[0x34, 0x12, 0x11, 0x11]);
        dev.config[4] = 3;
        dev.config[8..12].copy_from_slice(&[2, 0, 0, 3]);
        dev.config[0x10] = 8;
        dev.config[0x2c..0x30].copy_from_slice(&[0xf4, 0x1a, 0, 0x11]);
        dev.dispi[0] = 0xb0c5;
        dev.regs[SEQ + 2] = 3;
        dev.regs[SEQ + 4] = 2;
        dev.regs[GC + 5] = 0x10;
        dev.regs[GC + 6] = 0x0e;
        dev.regs[GC + 8] = 0xff;
        dev.regs[CRTC + 1] = 79;
        dev.regs[CRTC + 7] = 2;
        dev.regs[CRTC + 9] = 15;
        dev.regs[CRTC + 0x0a] = 0x20;
        dev.regs[CRTC + 0x12] = 0x8f;
        dev.regs[CRTC + 0x13] = 40;
        dev.regs[ATTR + 0x12] = 15;
        for i in 0..16 {
            dev.regs[ATTR + i] = i as u8;
            let bright = if i & 8 != 0 { 21 } else { 0 };
            dev.regs[DAC + i * 3] = if i & 4 != 0 { 42 } else { 0 } + bright;
            dev.regs[DAC + i * 3 + 1] = if i & 2 != 0 { 42 } else { 0 } + bright;
            dev.regs[DAC + i * 3 + 2] = if i & 1 != 0 { 42 } else { 0 } + bright;
        }
        dev.regs[DAC + 6 * 3 + 1] = 21;
        for ch in 0..256 {
            for y in 0..16 {
                dev.set_byte((ch * 32 + y) * 4 + 2, font::row(ch as u8, y));
            }
        }
        Ok(dev)
    }

    fn enabled(&self) -> bool {
        self.dispi[4] & 1 != 0
    }

    fn dispi_read(&self, index: usize) -> u16 {
        if index == 10 {
            return (VRAM_SIZE >> 16) as u16;
        }
        if self.dispi[4] & 2 != 0 {
            match index {
                1 => return 4096,
                2 => return 2160,
                3 => return 32,
                _ => {}
            }
        }
        if index == 7 {
            return (VRAM_SIZE / self.guest_stride().max(1)).min(u16::MAX as usize) as u16;
        }
        self.dispi.get(index).copied().unwrap_or(0)
    }

    fn guest_stride(&self) -> usize {
        let bpp = if self.dispi[3] == 15 {
            16
        } else {
            self.dispi[3]
        };
        (self.dispi[6] as usize * bpp as usize).div_ceil(8)
    }

    fn dispi_write(&mut self, index: usize, value: u16) {
        let valid = match index {
            0 => (0xb0c0..=0xb0c5).contains(&value),
            1 | 6 => value > 0 && value <= 4096,
            2 => value > 0 && value <= 2160,
            3 => matches!(value, 4 | 8 | 15 | 16 | 24 | 32),
            4 => true,
            5 => (value as usize) < VRAM_SIZE / 65536,
            8 | 9 => true,
            _ => false,
        };
        if !valid {
            return;
        }
        if index == 4 && value & 1 != 0 && !self.enabled() {
            if self.dispi[1] == 0 || self.dispi[2] == 0 || self.dispi[3] == 0 {
                return;
            }
            self.dispi[6] = self.dispi[1];
            self.dispi[8] = 0;
            self.dispi[9] = 0;
            if value & 0x80 == 0 {
                self.vram
                    .write_slice(&vec![0; VRAM_SIZE], GuestAddress(0))
                    .unwrap();
            }
        }
        if index == 6 && value < self.dispi[1] {
            return;
        }
        self.dispi[index] = value;
        if index == 1 && self.enabled() {
            self.dispi[6] = self.dispi[6].max(value);
        }
        self.dirty = true;
    }

    /// Absolute port I/O. Supports indexed VGA word writes and DISPI words.
    pub fn io_read(&mut self, port: u16, data: &mut [u8]) {
        if data.len() == 2 && matches!(port, 0x1ce..=0x1d0) {
            let value = if port == 0x1ce {
                self.dispi_index
            } else {
                self.dispi_read(self.dispi_index as usize)
            };
            data.copy_from_slice(&value.to_le_bytes());
            return;
        }
        for (i, byte) in data.iter_mut().enumerate() {
            *byte = self.port_read(port.wrapping_add(i as u16));
        }
    }

    pub fn io_write(&mut self, port: u16, data: &[u8]) {
        if data.len() == 2 && matches!(port, 0x1ce..=0x1d0) {
            let value = u16::from_le_bytes([data[0], data[1]]);
            if port == 0x1ce {
                self.dispi_index = value;
            } else {
                self.dispi_write(self.dispi_index as usize, value);
            }
            return;
        }
        for (i, byte) in data.iter().enumerate() {
            self.port_write(port.wrapping_add(i as u16), *byte);
        }
    }

    fn inactive_crtc(&self, port: u16) -> bool {
        ((0x3b0..=0x3bf).contains(&port) && self.misc & 1 != 0)
            || ((0x3d0..=0x3df).contains(&port) && self.misc & 1 == 0)
    }

    fn port_read(&mut self, port: u16) -> u8 {
        if self.inactive_crtc(port) {
            return 0xff;
        }
        match port {
            0x1ce => self.dispi_index as u8,
            0x1cf | 0x1d0 => self.dispi_read(self.dispi_index as usize) as u8,
            0x3c0 => self.attr_index,
            0x3c1 => self.regs[ATTR + (self.attr_index & 31) as usize],
            0x3c2 => 0x10,
            0x3c4 => self.seq_index,
            0x3c5 => self.regs[SEQ + self.seq_index as usize],
            0x3c6 => self.dac_mask,
            0x3c7 => self.dac_state,
            0x3c8 => self.dac_write,
            0x3c9 => {
                let value =
                    self.regs[DAC + self.dac_read as usize * 3 + self.dac_component as usize];
                self.dac_component += 1;
                if self.dac_component == 3 {
                    self.dac_component = 0;
                    self.dac_read = self.dac_read.wrapping_add(1);
                }
                value
            }
            0x3ca => self.feature,
            0x3cc => self.misc,
            0x3ce => self.gc_index,
            0x3cf => self.regs[GC + self.gc_index as usize],
            0x3b4 | 0x3d4 => self.crtc_index,
            0x3b5 | 0x3d5 => self.regs[CRTC + self.crtc_index as usize],
            0x3ba | 0x3da => {
                self.attr_data = false;
                // Alternate retrace states so firmware polling always progresses.
                self.status ^= 9;
                self.status
            }
            _ => 0xff,
        }
    }

    fn port_write(&mut self, port: u16, value: u8) {
        if self.inactive_crtc(port) {
            return;
        }
        self.dirty = true;
        match port {
            0x1ce => self.dispi_index = value as u16,
            0x1cf | 0x1d0 => self.dispi_write(self.dispi_index as usize, value as u16),
            0x3c0 => {
                if self.attr_data {
                    self.regs[ATTR + (self.attr_index & 31) as usize] = value;
                } else {
                    self.attr_index = value;
                }
                self.attr_data = !self.attr_data;
            }
            0x3c2 => self.misc = value,
            0x3c4 => self.seq_index = value,
            0x3c5 => self.regs[SEQ + self.seq_index as usize] = value,
            0x3c6 => self.dac_mask = value,
            0x3c7 => {
                self.dac_read = value;
                self.dac_component = 0;
                self.dac_state = 3;
            }
            0x3c8 => {
                self.dac_write = value;
                self.dac_component = 0;
                self.dac_state = 0;
            }
            0x3c9 => {
                self.regs[DAC + self.dac_write as usize * 3 + self.dac_component as usize] = value;
                self.dac_component += 1;
                if self.dac_component == 3 {
                    self.dac_component = 0;
                    self.dac_write = self.dac_write.wrapping_add(1);
                }
            }
            0x3ce => self.gc_index = value,
            0x3cf => self.regs[GC + self.gc_index as usize] = value,
            0x3b4 | 0x3d4 => self.crtc_index = value,
            0x3b5 | 0x3d5 => {
                let i = self.crtc_index as usize;
                if self.regs[CRTC + 0x11] & 0x80 == 0 || i > 7 {
                    self.regs[CRTC + i] = value;
                } else if i == 7 {
                    self.regs[CRTC + 7] = (self.regs[CRTC + 7] & !0x10) | (value & 0x10);
                }
            }
            0x3ba | 0x3da => self.feature = value,
            _ => {}
        }
    }

    fn legacy_offset(&self, offset: u64) -> Option<usize> {
        let (base, len) = match (self.regs[GC + 6] >> 2) & 3 {
            0 => (0, 0x20000),
            1 => (0, 0x10000),
            2 => (0x10000, 0x8000),
            _ => (0x18000, 0x8000),
        };
        offset
            .checked_sub(base)
            .filter(|v| *v < len)
            .map(|v| v as usize)
    }

    fn address(&self, offset: usize, write: bool) -> (usize, u8) {
        if self.regs[SEQ + 4] & 8 != 0 {
            (offset & !3, 1 << (offset & 3))
        } else if (write && self.regs[SEQ + 4] & 4 == 0)
            || (!write && self.regs[GC + 5] & 0x10 != 0)
        {
            ((offset & !1) * 2, 0x05 << (offset & 1))
        } else {
            ((offset & 0xffff) * 4, 0x0f)
        }
    }

    pub fn legacy_read(&mut self, offset: u64, data: &mut [u8]) {
        for (i, byte) in data.iter_mut().enumerate() {
            *byte = 0xff;
            let Some(off) = offset.checked_add(i as u64).filter(|v| *v < 0x20000) else {
                continue;
            };
            if self.enabled() && self.dispi[3] != 4 {
                if off < 65536 {
                    *byte = self.byte(self.dispi[5] as usize * 65536 + off as usize);
                }
                continue;
            }
            let Some(off) = self.legacy_offset(off) else {
                continue;
            };
            let (addr, mask) = self.address(off, false);
            self.vram
                .read_slice(&mut self.latch, GuestAddress(addr as u64))
                .unwrap();
            if self.regs[GC + 5] & 8 != 0 {
                let mut result = 0xff;
                for p in 0..4 {
                    if self.regs[GC + 7] & (1 << p) != 0 {
                        let compare = if self.regs[GC + 2] & (1 << p) != 0 {
                            0xff
                        } else {
                            0
                        };
                        result &= !(self.latch[p] ^ compare);
                    }
                }
                *byte = result;
            } else {
                let plane = if mask == 0x0f {
                    self.regs[GC + 4] & 3
                } else if self.regs[SEQ + 4] & 8 != 0 {
                    off as u8 & 3
                } else {
                    (self.regs[GC + 4] & 2) | (off as u8 & 1)
                };
                *byte = self.latch[plane as usize];
            }
        }
    }

    pub fn legacy_write(&mut self, offset: u64, data: &[u8]) {
        for (i, value) in data.iter().copied().enumerate() {
            let Some(off) = offset.checked_add(i as u64).filter(|v| *v < 0x20000) else {
                continue;
            };
            if self.enabled() && self.dispi[3] != 4 {
                if off < 65536 {
                    self.set_byte(self.dispi[5] as usize * 65536 + off as usize, value);
                    self.dirty = true;
                }
                continue;
            }
            let Some(off) = self.legacy_offset(off) else {
                continue;
            };
            let (addr, planes) = self.address(off, true);
            let rotated = value.rotate_right((self.regs[GC + 3] & 7) as u32);
            for p in 0..4 {
                if planes & self.regs[SEQ + 2] & (1 << p) == 0 {
                    continue;
                }
                let mode = self.regs[GC + 5] & 3;
                let set = if self.regs[GC] & (1 << p) != 0 {
                    0xff
                } else {
                    0
                };
                let source = match mode {
                    0 if self.regs[GC + 1] & (1 << p) != 0 => set,
                    0 => rotated,
                    1 => self.latch[p],
                    2 => {
                        if value & (1 << p) != 0 {
                            0xff
                        } else {
                            0
                        }
                    }
                    _ => set,
                };
                let source = if mode == 1 {
                    source
                } else {
                    match (self.regs[GC + 3] >> 3) & 3 {
                        1 => source & self.latch[p],
                        2 => source | self.latch[p],
                        3 => source ^ self.latch[p],
                        _ => source,
                    }
                };
                let mask = match mode {
                    1 => 0xff,
                    3 => rotated & self.regs[GC + 8],
                    _ => self.regs[GC + 8],
                };
                self.set_byte(addr + p, (source & mask) | (self.latch[p] & !mask));
            }
            self.dirty = true;
        }
    }

    /// Mode of the converted, tightly packed XRGB8888 frame.
    pub fn mode(&self) -> Mode {
        let (width, height) = if self.enabled() {
            (self.dispi[1] as u32, self.dispi[2] as u32)
        } else {
            let width =
                (self.regs[CRTC + 1] as u32 + 1) * if self.regs[SEQ + 1] & 1 != 0 { 8 } else { 9 };
            let overflow = self.regs[CRTC + 7] as u32;
            let height = self.regs[CRTC + 0x12] as u32
                + ((overflow & 2) << 7)
                + ((overflow & 0x40) << 3)
                + 1;
            if self.regs[GC + 5] & 0x40 != 0 {
                (
                    width / 2,
                    height / (self.regs[CRTC + 9] as u32 & 31).saturating_add(1),
                )
            } else {
                (width, height)
            }
        };
        Mode {
            width,
            height,
            stride: width * 4,
            format: PixelFormat::Xrgb8888,
        }
    }

    fn palette(&self, index: u8, attribute: bool) -> [u8; 4] {
        let index = if attribute {
            let value = self.regs[ATTR + (index & 15) as usize];
            let select = self.regs[ATTR + 0x14];
            let low = if self.regs[ATTR + 0x10] & 0x80 != 0 {
                (value & 15) | ((select & 3) << 4)
            } else {
                value & 63
            };
            low | ((select & 12) << 4)
        } else {
            index
        } & self.dac_mask;
        let mut rgb = [0; 4];
        for c in 0..3 {
            let value = self.regs[DAC + index as usize * 3 + c];
            rgb[2 - c] = if self.dispi[4] & 0x20 != 0 {
                value
            } else {
                ((value & 63) * 4) | ((value & 63) >> 4)
            };
        }
        rgb
    }

    fn byte(&self, addr: usize) -> u8 {
        if !self.render_vram.is_empty() {
            return self.render_vram.get(addr).copied().unwrap_or(0);
        }
        self.vram.read_obj(GuestAddress(addr as u64)).unwrap_or(0)
    }

    fn set_byte(&self, addr: usize, value: u8) {
        let _ = self.vram.write_obj(value, GuestAddress(addr as u64));
    }

    /// Page-aligned BAR0 storage, accessed with volatile guest-memory operations.
    /// The VMM may map this region directly while PCI memory decoding is enabled.
    pub fn linear_framebuffer(&self) -> GuestMemoryMmap {
        self.vram.clone()
    }

    /// Direct guest mappings bypass BAR callbacks; notify scanout of their writes.
    pub fn mark_framebuffer_dirty(&mut self) {
        self.dirty = true;
    }

    fn vbe_pixel(&self, x: usize, y: usize) -> [u8; 4] {
        let x = x + self.dispi[8] as usize;
        let y = y + self.dispi[9] as usize;
        let bpp = self.dispi[3];
        let addr = y * self.guest_stride() + x * (bpp as usize).div_ceil(8);
        match bpp {
            4 => self.planar_pixel(x, y, self.dispi[6] as usize / 8, 0),
            8 => self.palette(self.byte(addr), false),
            15 | 16 => {
                let bytes = [self.byte(addr), self.byte(addr + 1)];
                let value = if self.big_endian {
                    u16::from_be_bytes(bytes)
                } else {
                    u16::from_le_bytes(bytes)
                };
                let green_bits = if bpp == 16 { 6 } else { 5 };
                let r = ((value >> (5 + green_bits)) & 31) as u32 * 255 / 31;
                let g =
                    ((value >> 5) & ((1 << green_bits) - 1)) as u32 * 255 / ((1 << green_bits) - 1);
                let b = (value & 31) as u32 * 255 / 31;
                [b as u8, g as u8, r as u8, 0]
            }
            _ if self.big_endian => {
                let skip = usize::from(bpp == 32);
                [
                    self.byte(addr + skip + 2),
                    self.byte(addr + skip + 1),
                    self.byte(addr + skip),
                    0,
                ]
            }
            _ => [self.byte(addr), self.byte(addr + 1), self.byte(addr + 2), 0],
        }
    }

    fn planar_pixel(&self, x: usize, y: usize, stride: usize, start: usize) -> [u8; 4] {
        let addr = ((start + y * stride + x / 8) & 0xffff) * 4;
        let mut index = 0;
        for p in 0..4 {
            index |= ((self.byte(addr + p) >> (7 - (x & 7))) & 1) << p;
        }
        self.palette(index & self.regs[ATTR + 0x12], true)
    }

    fn legacy_pixel(&self, x: usize, y: usize) -> [u8; 4] {
        let start = ((self.regs[CRTC + 0x0c] as usize) << 8) | self.regs[CRTC + 0x0d] as usize;
        let stride = self.regs[CRTC + 0x13] as usize * 2;
        if self.regs[GC + 6] & 1 == 0 {
            let cw = if self.regs[SEQ + 1] & 1 != 0 { 8 } else { 9 };
            let ch = (self.regs[CRTC + 9] as usize & 31) + 1;
            let row = y + (self.regs[CRTC + 8] as usize & 31);
            let addr = (start + row / ch * stride + x / cw) & 0xffff;
            let code = self.byte(addr * 4);
            let attr = self.byte(addr * 4 + 1);
            let select = self.regs[SEQ + 3];
            let bank = if attr & 8 != 0 {
                ((select >> 2) & 3) | ((select >> 3) & 4)
            } else {
                (select & 3) | ((select >> 2) & 4)
            } as usize;
            let glyph = self.byte((bank * 8192 + code as usize * 32 + row % ch) * 4 + 2);
            let dot = x % cw;
            let ink = if dot < 8 {
                glyph & (0x80 >> dot) != 0
            } else {
                (0xc0..=0xdf).contains(&code) && self.regs[ATTR + 0x10] & 4 != 0 && glyph & 1 != 0
            };
            let cursor = ((self.regs[CRTC + 0x0e] as usize) << 8) | self.regs[CRTC + 0x0f] as usize;
            let cursor_on = self.regs[CRTC + 0x0a] & 0x20 == 0
                && addr == cursor
                && row % ch >= (self.regs[CRTC + 0x0a] & 31) as usize
                && row % ch <= (self.regs[CRTC + 0x0b] & 31) as usize;
            let bg = if self.regs[ATTR + 0x10] & 8 != 0 {
                (attr >> 4) & 7
            } else {
                attr >> 4
            };
            self.palette(if ink || cursor_on { attr & 15 } else { bg }, true)
        } else if self.regs[GC + 5] & 0x40 != 0 {
            self.palette(self.byte(start * 4 + y * stride * 4 + x), false)
        } else {
            self.planar_pixel(
                x,
                y / if self.regs[CRTC + 9] & 0x80 != 0 {
                    2
                } else {
                    1
                },
                stride,
                start,
            )
        }
    }

    /// Convert the visible framebuffer and consume full-frame damage.
    /// The caller may retain the returned pixels across subsequent device writes.
    pub fn frame(&mut self) -> Frame {
        let mode = self.mode();
        if !self.dirty
            && let Some(pixels) = &self.cached_pixels
        {
            return Frame {
                mode,
                pixels: pixels.clone(),
                dirty: false,
            };
        }
        let mut pixels = vec![0; mode.stride as usize * mode.height as usize];
        // Translate and copy guest memory once, rather than once per pixel.
        let size = if self.enabled() { VRAM_SIZE } else { 0x40000 };
        self.render_vram.resize(size, 0);
        self.vram
            .read_slice(&mut self.render_vram, GuestAddress(0))
            .expect("VGA rendering storage is mapped");
        let blank =
            !self.enabled() && (self.regs[SEQ + 1] & 0x20 != 0 || self.attr_index & 0x20 == 0);
        if !blank {
            for y in 0..mode.height as usize {
                for x in 0..mode.width as usize {
                    let pixel = if self.enabled() {
                        self.vbe_pixel(x, y)
                    } else {
                        self.legacy_pixel(x, y)
                    };
                    let pos = y * mode.stride as usize + x * 4;
                    pixels[pos..pos + 4].copy_from_slice(&pixel);
                }
            }
        }
        self.cached_pixels = Some(pixels.clone());
        self.render_vram.clear();
        Frame {
            mode,
            pixels,
            dirty: std::mem::replace(&mut self.dirty, false),
        }
    }
}

impl PciDevice for StandardVga {
    fn config_read(&mut self, reg: usize) -> u32 {
        let Some(bytes) = reg
            .checked_mul(4)
            .and_then(|off| self.config.get(off..off + 4))
        else {
            return 0;
        };
        u32::from_le_bytes(bytes.try_into().expect("config dword"))
    }

    fn config_write(&mut self, reg: usize, offset: u64, data: &[u8]) {
        let Some(start) = reg
            .checked_mul(4)
            .and_then(|v| v.checked_add(usize::try_from(offset).ok()?))
        else {
            return;
        };
        for (i, byte) in data.iter().enumerate() {
            let Some(pos) = start.checked_add(i).filter(|v| *v < 256) else {
                continue;
            };
            match pos {
                4 => self.config[pos] = byte & 0x23,
                5 => self.config[pos] = byte & 1,
                0x0c | 0x0d => self.config[pos] = *byte,
                0x10..=0x13 | 0x18..=0x1b | 0x30..=0x33 => self.config[pos] = *byte,
                _ => {}
            }
        }
        for (off, mask, flags) in [
            (0x10, 0xff000000u32, 8u32),
            (0x18, 0xfffff000, 0),
            (0x30, 0xfffe0001, 0),
        ] {
            let value =
                u32::from_le_bytes(self.config[off..off + 4].try_into().expect("BAR dword"));
            self.config[off..off + 4].copy_from_slice(&((value & mask) | flags).to_le_bytes());
        }
    }

    fn bars(&self) -> Vec<BarInfo> {
        vec![
            BarInfo {
                index: 0,
                size: VRAM_SIZE as u64,
                kind: BarKind::Mmio32,
            },
            BarInfo {
                index: 2,
                size: 4096,
                kind: BarKind::Mmio32,
            },
            BarInfo {
                index: ROM_BAR,
                size: ROM_SIZE as u64,
                kind: BarKind::Mmio32,
            },
        ]
    }

    fn bar_read(&mut self, bar: u8, offset: u64, data: &mut [u8]) {
        data.fill(0xff);
        if bar == 2 {
            if (0x400..0x420).contains(&offset) {
                self.io_read((offset - 0x400 + 0x3c0) as u16, data);
            } else if (0x500..0x516).contains(&offset)
                && data.len() <= 2
                && (offset & 1) as usize + data.len() <= 2
            {
                let bytes = self
                    .dispi_read(((offset - 0x500) / 2) as usize)
                    .to_le_bytes();
                data.copy_from_slice(
                    &bytes[(offset & 1) as usize..(offset & 1) as usize + data.len()],
                );
            } else if (0x600..0x608).contains(&offset)
                && data.len() <= 4
                && (offset & 3) as usize + data.len() <= 4
            {
                let value: u32 = if offset < 0x604 {
                    8
                } else if self.big_endian {
                    0xbebebebe
                } else {
                    0x1e1e1e1e
                };
                let bytes = value.to_le_bytes();
                data.copy_from_slice(
                    &bytes[(offset & 3) as usize..(offset & 3) as usize + data.len()],
                );
            } else {
                data.fill(0);
            }
            return;
        }
        if bar == 0 {
            for (i, byte) in data.iter_mut().enumerate() {
                *byte = offset
                    .checked_add(i as u64)
                    .and_then(|v| usize::try_from(v).ok())
                    .map_or(0xff, |addr| self.byte(addr));
            }
            return;
        }
        let source = match bar {
            ROM_BAR => &self.rom,
            _ => return,
        };
        for (i, byte) in data.iter_mut().enumerate() {
            if let Some(value) = offset
                .checked_add(i as u64)
                .and_then(|v| usize::try_from(v).ok())
                .and_then(|v| source.get(v))
            {
                *byte = *value;
            }
        }
    }

    fn bar_write(&mut self, bar: u8, offset: u64, data: &[u8]) {
        match bar {
            0 => {
                for (i, byte) in data.iter().enumerate() {
                    if let Some(dest) = offset
                        .checked_add(i as u64)
                        .and_then(|v| usize::try_from(v).ok())
                        .filter(|v| *v < VRAM_SIZE)
                    {
                        self.set_byte(dest, *byte);
                        self.dirty = true;
                    }
                }
            }
            2 if (0x400..0x420).contains(&offset) => {
                self.io_write((offset - 0x400 + 0x3c0) as u16, data)
            }
            2 if (0x500..0x516).contains(&offset)
                && data.len() <= 2
                && (offset & 1) as usize + data.len() <= 2 =>
            {
                let index = ((offset - 0x500) / 2) as usize;
                let mut bytes = self.dispi_read(index).to_le_bytes();
                bytes[(offset & 1) as usize..(offset & 1) as usize + data.len()]
                    .copy_from_slice(data);
                self.dispi_write(index, u16::from_le_bytes(bytes));
            }
            2 if offset == 0x604 && data.len() == 4 => {
                match u32::from_le_bytes(data.try_into().expect("extended register")) {
                    0xbebebebe => self.big_endian = true,
                    0x1e1e1e1e => self.big_endian = false,
                    _ => return,
                }
                self.dirty = true;
            }
            _ => {}
        }
    }
}

impl Snapshot for StandardVga {
    fn state_id(&self) -> String {
        self.id.clone()
    }

    fn save_state(&self) -> Result<Vec<u8>, DeviceError> {
        let mut state = b"VGA\0\x01\0\0\0".to_vec();
        state.extend_from_slice(&self.config);
        state.extend_from_slice(&self.regs);
        for value in self.dispi {
            state.extend_from_slice(&value.to_le_bytes());
        }
        state.extend_from_slice(&[
            self.seq_index,
            self.gc_index,
            self.crtc_index,
            self.attr_index,
            self.attr_data as u8,
            self.misc,
            self.feature,
            self.dac_mask,
            self.dac_read,
            self.dac_write,
            self.dac_component,
            self.dac_state,
            self.status,
            self.big_endian as u8,
        ]);
        state.extend_from_slice(&self.dispi_index.to_le_bytes());
        state.extend_from_slice(&self.latch);
        let start = state.len();
        state.resize(start + VRAM_SIZE, 0);
        self.vram
            .read_slice(&mut state[start..], GuestAddress(0))
            .map_err(|e| DeviceError::Memory(e.to_string()))?;
        Ok(state)
    }

    fn restore_state(&mut self, state: &[u8]) -> Result<(), DeviceError> {
        let header = 8 + 256 + REG_SIZE + 32;
        if state.len() != header + 20 + VRAM_SIZE || &state[..8] != b"VGA\0\x01\0\0\0" {
            return Err(DeviceError::State(
                "invalid VGA snapshot version or length".into(),
            ));
        }
        let scalar = &state[header..header + 20];
        let mut dispi = [0; 16];
        for (i, value) in dispi.iter_mut().enumerate() {
            let at = 8 + 256 + REG_SIZE + i * 2;
            *value = u16::from_le_bytes([state[at], state[at + 1]]);
        }
        if scalar[4] > 1
            || scalar[10] > 2
            || scalar[13] > 1
            || dispi[1] > 4096
            || dispi[2] > 2160
            || dispi[6] > 4096
            || dispi[5] >= 256
            || !matches!(dispi[3], 0 | 4 | 8 | 15 | 16 | 24 | 32)
            || (dispi[4] & 1 != 0
                && (dispi[1] == 0 || dispi[2] == 0 || dispi[3] == 0 || dispi[6] < dispi[1]))
        {
            return Err(DeviceError::State("invalid VGA register state".into()));
        }
        self.config.copy_from_slice(&state[8..264]);
        self.regs.copy_from_slice(&state[264..264 + REG_SIZE]);
        self.dispi = dispi;
        self.seq_index = scalar[0];
        self.gc_index = scalar[1];
        self.crtc_index = scalar[2];
        self.attr_index = scalar[3];
        self.attr_data = scalar[4] != 0;
        self.misc = scalar[5];
        self.feature = scalar[6];
        self.dac_mask = scalar[7];
        self.dac_read = scalar[8];
        self.dac_write = scalar[9];
        self.dac_component = scalar[10];
        self.dac_state = scalar[11];
        self.status = scalar[12];
        self.big_endian = scalar[13] != 0;
        self.dispi_index = u16::from_le_bytes([scalar[14], scalar[15]]);
        self.latch.copy_from_slice(&scalar[16..20]);
        self.vram
            .write_slice(&state[header + 20..], GuestAddress(0))
            .map_err(|e| DeviceError::Memory(e.to_string()))?;
        self.dirty = true;
        Ok(())
    }
}
