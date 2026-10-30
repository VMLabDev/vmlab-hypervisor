//! Captured 1af4:1050 virtio-vga PCI function.
//!
//! Route VGA ports and the legacy aperture as for `StandardVga`. BAR2 includes
//! VGA MMIO at 0x0000, common config at 0x1000, ISR at 0x1800, device config
//! at 0x2000 and notifications at 0x3000. BAR4 holds MSI-X, with PBA at 0x800.
//! BAR0 is 8 MiB and prefetchable. Preserve its config-space bit 3 in the root.
//! ROM_BAR is the expansion ROM. Load `/usr/share/qemu/vgabios-virtio.bin`
//! from SeaVGABIOS on the host and pass its bytes to `new`.

use super::{Frame, Mode, ROM_BAR, ROM_SIZE, StandardVga};
use crate::virtio::{
    gpu::{Cursor, Gpu},
    pci::VirtioPciDevice,
};
use crate::{
    BarInfo, BarKind, DeviceError, GuestMemory, IrqLine, MsiSink, PciDevice, PciSnapshot, Snapshot,
};
use std::sync::Arc;

const VGA_SIZE: u64 = 8 * 1024 * 1024;

/// VGA compatibility and a modern 2D GPU in one PCI function.
pub struct VirtioVga {
    id: String,
    vga: StandardVga,
    gpu: Gpu,
    transport: VirtioPciDevice,
    config: [u8; 256],
}

impl VirtioVga {
    pub fn new(
        id: impl Into<String>,
        rom: &[u8],
        mem: GuestMemory,
        msi: Option<Arc<dyn MsiSink>>,
        intx: Option<Arc<dyn IrqLine>>,
    ) -> Result<Self, DeviceError> {
        let id = id.into();
        let vga = StandardVga::new(format!("{id}.vga"), rom)?;
        let gpu = Gpu::new(format!("{id}.gpu"));
        let transport = VirtioPciDevice::new(
            format!("{id}.transport"),
            Box::new(gpu.clone()),
            mem,
            msi,
            intx,
        );
        let mut config = [0; 256];
        config[..4].copy_from_slice(&[0xf4, 0x1a, 0x50, 0x10]);
        config[6] = 0x10;
        config[8..12].copy_from_slice(&[1, 0, 0, 3]);
        config[0x10] = 8;
        config[0x18] = 12;
        config[0x2c..0x30].copy_from_slice(&[0xf4, 0x1a, 0, 0x11]);
        config[0x34] = 0x98;
        config[0x3d] = 1;
        for (at, next, kind, offset, len) in [
            (0x40, 0, 1, 0x1000u32, 0x800u32),
            (0x50, 0x40, 3, 0x1800, 0x800),
            (0x60, 0x50, 4, 0x2000, 0x1000),
            (0x70, 0x60, 2, 0x3000, 0x1000),
            (0x84, 0x70, 5, 0, 0),
        ] {
            config[at..at + 5].copy_from_slice(&[
                9,
                next,
                if kind == 2 || kind == 5 { 20 } else { 16 },
                kind,
                if kind == 5 { 0 } else { 2 },
            ]);
            config[at + 8..at + 12].copy_from_slice(&offset.to_le_bytes());
            config[at + 12..at + 16].copy_from_slice(&len.to_le_bytes());
        }
        config[0x80..0x84].copy_from_slice(&4u32.to_le_bytes());
        config[0x98..0x9c].copy_from_slice(&[0x11, 0x84, 2, 0]);
        config[0x9c..0xa0].copy_from_slice(&4u32.to_le_bytes());
        config[0xa0..0xa4].copy_from_slice(&0x804u32.to_le_bytes());
        Ok(Self {
            id,
            vga,
            gpu,
            transport,
            config,
        })
    }

    pub fn io_read(&mut self, port: u16, data: &mut [u8]) {
        self.vga.io_read(port, data);
        if matches!(port, 0x1cf | 0x1d0)
            && let Some(value) = self.vram_register(self.vga.dispi_index)
        {
            let value = value.to_le_bytes();
            let n = data.len().min(2);
            data[..n].copy_from_slice(&value[..n]);
        }
    }
    pub fn io_write(&mut self, port: u16, data: &[u8]) {
        let bank = self.vga.dispi[5];
        self.vga.io_write(port, data);
        if self.vga.dispi[5] >= (VGA_SIZE >> 16) as u16 {
            self.vga.dispi[5] = bank;
        }
    }
    pub fn legacy_read(&mut self, offset: u64, data: &mut [u8]) {
        self.vga.legacy_read(offset, data);
    }
    pub fn legacy_write(&mut self, offset: u64, data: &[u8]) {
        self.vga.legacy_write(offset, data);
    }
    pub fn mode(&self) -> Mode {
        self.gpu.mode().unwrap_or_else(|| self.vga.mode())
    }
    pub fn frame(&mut self) -> Frame {
        self.gpu.frame().unwrap_or_else(|| self.vga.frame())
    }
    pub fn cursor(&self) -> Cursor {
        self.gpu.cursor()
    }
    pub fn request_size(&self, width: u32, height: u32) -> Result<(), DeviceError> {
        self.gpu.request_size(width, height)
    }

    fn vram_register(&self, index: u16) -> Option<u16> {
        match index {
            7 => Some(
                (VGA_SIZE as usize / self.vga.guest_stride().max(1)).min(u16::MAX as usize) as u16,
            ),
            10 => Some((VGA_SIZE >> 16) as u16),
            _ => None,
        }
    }

    fn cfg_access(&mut self, write: Option<&[u8]>, data: &mut [u8]) {
        let bar = self.config[0x88];
        let offset = u32::from_le_bytes(self.config[0x8c..0x90].try_into().unwrap()) as u64;
        let len = u32::from_le_bytes(self.config[0x90..0x94].try_into().unwrap()) as usize;
        if !matches!(len, 1 | 2 | 4) || offset & (len as u64 - 1) != 0 {
            return;
        }
        if let Some(bytes) = write {
            if bytes.len() == len {
                self.bar_write(bar, offset, bytes);
            }
        } else if data.len() == len {
            self.bar_read(bar, offset, data);
        }
    }
}

impl PciDevice for VirtioVga {
    fn config_read(&mut self, reg: usize) -> u32 {
        if reg >= 64 {
            return 0;
        }
        match reg * 4 {
            4 => self.transport.config_read(1),
            0x98 => (self.transport.config_read(0x40 / 4) & 0xffff0000) | 0x8411,
            0x94 => {
                let mut data = [0; 4];
                let len = u32::from_le_bytes(self.config[0x90..0x94].try_into().unwrap()) as usize;
                if matches!(len, 1 | 2 | 4) {
                    self.cfg_access(None, &mut data[..len]);
                }
                u32::from_le_bytes(data)
            }
            _ => u32::from_le_bytes(self.config[reg * 4..reg * 4 + 4].try_into().unwrap()),
        }
    }
    fn config_write(&mut self, reg: usize, offset: u64, data: &[u8]) {
        if reg >= 64 || offset > 3 || data.len() > 4 - offset as usize {
            return;
        }
        let at = reg * 4 + offset as usize;
        if reg == 1 {
            self.transport.config_write(reg, offset, data);
        }
        if at < 0x9c && at + data.len() > 0x9a {
            // The transport's MSI-X capability lives at 0x40.
            let mut control = (self.transport.config_read(16) >> 16) as u16;
            let mut bytes = control.to_le_bytes();
            for (i, byte) in data.iter().enumerate() {
                if (0x9a..0x9c).contains(&(at + i)) {
                    bytes[at + i - 0x9a] = *byte;
                }
            }
            control = u16::from_le_bytes(bytes);
            self.transport.config_write(16, 2, &control.to_le_bytes());
        }
        for (i, byte) in data.iter().enumerate() {
            if let p @ (0x0c..=0x0d
            | 0x10..=0x13
            | 0x18..=0x23
            | 0x30..=0x33
            | 0x3c
            | 0x88
            | 0x8c..=0x93) = at + i
            {
                self.config[p] = *byte;
            }
        }
        for (at, mask, flags) in [
            (0x10, 0xff800000u32, 8),
            (0x18, 0xffffc000, 12),
            (0x20, 0xfffff000, 0),
            (0x30, 0xfffe0001, 0),
        ] {
            let value = u32::from_le_bytes(self.config[at..at + 4].try_into().unwrap());
            self.config[at..at + 4].copy_from_slice(&((value & mask) | flags).to_le_bytes());
        }
        if reg == 0x94 / 4 {
            let len = u32::from_le_bytes(self.config[0x90..0x94].try_into().unwrap()) as usize;
            if offset == 0 && data.len() == len {
                self.cfg_access(Some(data), &mut []);
            }
        }
    }
    fn bars(&self) -> Vec<BarInfo> {
        vec![
            BarInfo {
                index: 0,
                size: VGA_SIZE,
                kind: BarKind::Mmio32,
            },
            BarInfo {
                index: 2,
                size: 0x4000,
                kind: BarKind::Mmio64 { prefetchable: true },
            },
            BarInfo {
                index: 4,
                size: 0x1000,
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
        if bar == ROM_BAR
            || (bar == 0
                && offset
                    .checked_add(data.len() as u64)
                    .is_some_and(|v| v <= VGA_SIZE))
        {
            self.vga.bar_read(bar, offset, data);
        } else if bar == 2 && offset < 0x1000 {
            self.vga.bar_read(2, offset, data);
            if (0x500..0x520).contains(&offset)
                && data.len() <= 2 - (offset & 1) as usize
                && let Some(value) = self.vram_register(((offset - 0x500) / 2) as u16)
            {
                let bytes = value.to_le_bytes();
                data.copy_from_slice(
                    &bytes[(offset & 1) as usize..(offset & 1) as usize + data.len()],
                );
            }
        } else if let Some(off) = transport_offset(bar, offset, data.len()) {
            self.transport.bar_read(4, off, data);
        }
    }
    fn bar_write(&mut self, bar: u8, offset: u64, data: &[u8]) {
        if bar == 0
            && offset
                .checked_add(data.len() as u64)
                .is_some_and(|v| v <= VGA_SIZE)
        {
            self.vga.bar_write(0, offset, data);
        } else if bar == 2 && offset < 0x1000 {
            let bank = self.vga.dispi[5];
            self.vga.bar_write(2, offset, data);
            if self.vga.dispi[5] >= (VGA_SIZE >> 16) as u16 {
                self.vga.dispi[5] = bank;
            }
        } else if let Some(off) = transport_offset(bar, offset, data.len()) {
            self.transport.bar_write(4, off, data);
            if !self.gpu.native() {
                self.vga.dirty = true;
            }
        }
    }
}

fn transport_offset(bar: u8, off: u64, len: usize) -> Option<u64> {
    let (start, end, dest) = match (bar, off) {
        (2, 0x1000..0x1800) => (0x1000, 0x1800, 0),
        (2, 0x1800..0x2000) => (0x1800, 0x2000, 0x2000),
        (2, 0x2000..0x3000) => (0x2000, 0x3000, 0x3000),
        (2, 0x3000..0x4000) => (0x3000, 0x4000, 0x1000),
        (4, 0..0x30) => (0, 0x30, 0x4000),
        (4, 0x800..0x808) => (0x800, 0x808, 0x4040),
        _ => return None,
    };
    off.checked_add(len as u64)
        .filter(|v| *v <= end)
        .map(|_| off - start + dest)
}

impl Snapshot for VirtioVga {
    fn state_id(&self) -> String {
        self.id.clone()
    }
    fn save_state(&self) -> Result<Vec<u8>, DeviceError> {
        let mut out = b"VVGA\x01\0\0\0".to_vec();
        out.extend_from_slice(&self.config);
        for state in [
            self.vga.save_state()?,
            self.transport.save_state()?,
            self.gpu.save_state()?,
        ] {
            out.extend_from_slice(&(state.len() as u32).to_le_bytes());
            out.extend_from_slice(&state);
        }
        Ok(out)
    }
    fn restore_state(&mut self, state: &[u8]) -> Result<(), DeviceError> {
        let bad = || DeviceError::State("invalid virtio-vga snapshot".into());
        if state.len() < 264 || &state[..8] != b"VVGA\x01\0\0\0" {
            return Err(bad());
        }
        let mut pos = 264;
        let mut sections = Vec::new();
        for _ in 0..3 {
            let bytes = state.get(pos..pos + 4).ok_or_else(bad)?;
            let len = u32::from_le_bytes(bytes.try_into().unwrap()) as usize;
            pos += 4;
            sections.push(
                state
                    .get(pos..pos.checked_add(len).ok_or_else(bad)?)
                    .ok_or_else(bad)?,
            );
            pos += len;
        }
        if pos != state.len() {
            return Err(bad());
        }
        self.vga.restore_state(sections[0])?;
        self.transport.restore_state(sections[1])?;
        self.gpu.restore_state(sections[2])?;
        self.config.copy_from_slice(&state[8..264]);
        Ok(())
    }
}

impl PciSnapshot for VirtioVga {
    fn snapshot_id(&self) -> String {
        self.id.clone()
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
        if !state.is_empty() {
            return Err(DeviceError::State(
                "unexpected virtio-vga wrapped state".into(),
            ));
        }
        Ok(())
    }
}

#[cfg(test)]
#[path = "../virtio/gpu/vga_tests.rs"]
mod tests;
