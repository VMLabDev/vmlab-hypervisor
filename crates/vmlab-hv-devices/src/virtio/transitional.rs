//! Transitional PCI identity and VIRTIO 1.2 §4.1.4.8 legacy registers.

use super::*;

/// Expansion ROM BAR index, shared with the PCI root and VGA devices.
pub const ROM_BAR: u8 = 6;
/// Captured virtio-net expansion ROM aperture.
pub const ROM_SIZE: usize = 256 * 1024;

pub(super) struct LegacyPci {
    pub(super) bar0: u32,
    pub(super) bar1: u32,
    pub(super) rom: u32,
    pub(super) rom_image: Option<Vec<u8>>,
    pub(super) io_size: u64,
    pub(super) active: bool,
    pub(super) pci_cfg: [u8; 16],
}

impl LegacyPci {
    pub(super) fn bars(&self) -> Vec<BarInfo> {
        let mut bars = vec![
            BarInfo {
                index: 0,
                size: self.io_size,
                kind: BarKind::Io,
            },
            BarInfo {
                index: 1,
                size: 0x1000,
                kind: BarKind::Mmio32,
            },
            BarInfo {
                index: 4,
                size: 0x4000,
                kind: BarKind::Mmio64 { prefetchable: true },
            },
        ];
        if self.rom_image.is_some() {
            bars.push(BarInfo {
                index: ROM_BAR,
                size: ROM_SIZE as u64,
                kind: BarKind::Mmio32,
            });
        }
        bars
    }
}

/// Translate the captured BAR4 layout to the original modern implementation.
pub(super) fn modern_offset(offset: u64) -> Option<u64> {
    match offset {
        0..=0xfff => Some(offset),
        0x1000..=0x1fff => Some(ISR_OFF + offset - 0x1000),
        0x2000..=0x2fff => Some(DEVICE_CFG_OFF + offset - 0x2000),
        0x3000..=0x3fff => Some(NOTIFY_OFF + offset - 0x3000),
        _ => None,
    }
}

impl VirtioPciDevice {
    /// Attach an upstream virtio-net PCI ROM to a transitional NIC. BAR 6
    /// supplies immutable bytes to the PCI root, which gates guest reads on
    /// the ROM enable and PCI memory-space bits, as for StandardVga.
    pub fn with_option_rom(mut self, image: &[u8]) -> Result<Self, DeviceError> {
        if self.transitional.is_none() || self.device.device_type() != 1 {
            return Err(DeviceError::Other(
                "option ROM requires transitional virtio-net".into(),
            ));
        }
        validate_net_rom(image)?;
        let mut bytes = vec![0xff; ROM_SIZE];
        bytes[..image.len()].copy_from_slice(image);
        self.transitional.as_mut().unwrap().rom_image = Some(bytes);
        Ok(self)
    }

    /// Wrap a net (1), block (2), or console (3) device in the captured
    /// transitional layout. `msix_vectors` is independent of queue count:
    /// captures use 2 for serial, 4 for net, and vCPU count + 1 for block.
    /// Multiple queues may share a vector. The existing `new` stays modern.
    ///
    /// Filesystem devices have no standard legacy PCI identity and must use
    /// `new`; no filesystem identity is present in the hardware captures.
    pub fn new_transitional(
        id: impl Into<String>,
        device: Box<dyn VirtioDevice>,
        mem: GuestMemory,
        msi: Option<Arc<dyn MsiSink>>,
        intx: Option<Arc<dyn IrqLine>>,
        msix_vectors: u16,
    ) -> Result<Self, DeviceError> {
        let (pci_id, class, io_size) = match device.device_type() {
            1 => (
                0x1000u16,
                0x0200_0000u32,
                if device.features() & (1 << 3) != 0 {
                    64
                } else {
                    32
                },
            ),
            2 => (0x1001, 0x0100_0000, 128),
            3 => (0x1003, 0x0780_0000, 64),
            ty => {
                return Err(DeviceError::Other(format!(
                    "virtio type {ty} has no supported transitional PCI identity"
                )));
            }
        };
        if !(1..=64).contains(&msix_vectors)
            || (device.device_type() != 1 && device.config_len() > io_size - 24)
        {
            return Err(DeviceError::Other(
                "invalid transitional vector count or configuration size".into(),
            ));
        }
        let mut pci = Self::new(id, device, mem, msi, intx);
        let config = &mut pci.config;
        config[2..4].copy_from_slice(&pci_id.to_le_bytes());
        config[8..12].copy_from_slice(&class.to_le_bytes());
        config[0x34] = 0x98;
        config[0x3d] = 1;
        config[0x40..].fill(0);
        for (off, next, ty, region) in [
            (0x40, 0, 1, 0),
            (0x50, 0x40, 3, 0x1000),
            (0x60, 0x50, 4, 0x2000),
            (0x70, 0x60, 2, 0x3000),
        ] {
            write_virtio_cap(config, off, next, ty, region, 0x1000);
        }
        config[0x72] = 20;
        config[0x80..0x84].copy_from_slice(&4u32.to_le_bytes());
        config[0x84..0x88].copy_from_slice(&[9, 0x70, 20, 5]);
        config[0x98..0x9c].copy_from_slice(&[0x11, 0x84, (msix_vectors - 1) as u8, 0]);
        config[0x9c..0xa0].copy_from_slice(&1u32.to_le_bytes());
        config[0xa0..0xa4].copy_from_slice(&0x801u32.to_le_bytes());
        // PCI MSI-X entries start masked (PCI Local Bus MSI-X specification).
        pci.int.lock().entries = vec![
            MsixEntry {
                masked: true,
                ..MsixEntry::default()
            };
            msix_vectors as usize
        ];
        pci.transitional = Some(LegacyPci {
            bar0: 0,
            bar1: 0,
            rom: 0,
            rom_image: None,
            io_size,
            active: false,
            pci_cfg: [0; 16],
        });
        Ok(pci)
    }

    pub(super) fn transitional_config_read(&mut self, reg: usize) -> u32 {
        let legacy = self.transitional.as_ref().unwrap();
        match reg * 4 {
            4 => {
                self.command as u32 | 0x10_0000 | {
                    let int = self.int.lock();
                    if !int.msix_enabled && int.isr != 0 {
                        0x8_0000
                    } else {
                        0
                    }
                }
            }
            0x10 => bar_value(legacy.bar0 as u64, legacy.io_size, 1, 0xffff_fffc),
            0x14 => bar_value(legacy.bar1 as u64, 0x1000, 0, 0xffff_fff0),
            0x20 => bar_value(self.bar4, 0x4000, BAR64_PREFETCHABLE, 0xffff_fff0),
            0x24 => (self.bar4 >> 32) as u32,
            0x30 if legacy.rom_image.is_some() => {
                if legacy.rom & 0xffff_f800 == 0xffff_f800 {
                    0xfffc_0000
                } else {
                    legacy.rom & 0xfffc_0001
                }
            }
            0x30 => 0,
            0x3c => self.interrupt_line as u32 | 0x100,
            0x88..=0x90 => {
                let off = reg * 4 - 0x88;
                u32::from_le_bytes(legacy.pci_cfg[off..off + 4].try_into().unwrap())
            }
            0x94 => {
                let cfg = legacy.pci_cfg;
                let len = u32::from_le_bytes(cfg[8..12].try_into().unwrap());
                let offset = u32::from_le_bytes(cfg[4..8].try_into().unwrap());
                let mut data = [0u8; 4];
                if matches!(len, 1 | 2 | 4) {
                    self.bar_read(cfg[0], offset as u64, &mut data[..len as usize]);
                }
                u32::from_le_bytes(data)
            }
            0x98 => 0x8411 | ((self.msix_control() as u32) << 16),
            _ => u32::from_le_bytes(self.config[reg * 4..reg * 4 + 4].try_into().unwrap()),
        }
    }

    pub(super) fn transitional_config_write(&mut self, reg: usize, offset: u64, data: &[u8]) {
        if reg >= PCI_CONFIG_SIZE / 4 || offset > 3 || data.len() > 4 - offset as usize {
            return;
        }
        let addr = reg * 4 + offset as usize;
        let mut control = self.msix_control().to_le_bytes();
        let mut write_control = false;
        let legacy = self.transitional.as_mut().unwrap();
        for (i, byte) in data.iter().copied().enumerate() {
            let pos = addr + i;
            match pos {
                4..=5 => {
                    set_byte16(&mut self.command, pos - 4, byte);
                    self.command &= 0x0407;
                }
                0x10..=0x13 => set_byte32(&mut legacy.bar0, pos - 0x10, byte),
                0x14..=0x17 => set_byte32(&mut legacy.bar1, pos - 0x14, byte),
                0x20..=0x27 => {
                    let shift = (pos - 0x20) * 8;
                    self.bar4 = (self.bar4 & !(0xff << shift)) | ((byte as u64) << shift);
                }
                0x30..=0x33 if legacy.rom_image.is_some() => {
                    set_byte32(&mut legacy.rom, pos - 0x30, byte)
                }
                0x3c => self.interrupt_line = byte,
                0x88 => legacy.pci_cfg[0] = byte,
                0x8c..=0x93 => legacy.pci_cfg[pos - 0x88] = byte,
                0x9a..=0x9b => {
                    control[pos - 0x9a] = byte;
                    write_control = true;
                }
                _ => {}
            }
        }
        if write_control {
            self.msix_control_write(u16::from_le_bytes(control));
        }
        if addr >= 0x94 && addr + data.len() <= 0x98 {
            let cfg = self.transitional.as_ref().unwrap().pci_cfg;
            let len = u32::from_le_bytes(cfg[8..12].try_into().unwrap()) as usize;
            let offset = u32::from_le_bytes(cfg[4..8].try_into().unwrap()) as u64;
            if matches!(len, 1 | 2 | 4) && addr == 0x94 && len == data.len() {
                self.bar_write(cfg[0], offset, data);
            }
        }
    }

    pub(super) fn legacy_read(&mut self, offset: u64, data: &mut [u8]) {
        let config_off = if self.int.lock().msix_enabled { 24 } else { 20 };
        if offset >= config_off {
            data.fill(0);
            let rel = offset - config_off;
            let config_len = self.legacy_config_len();
            if rel < config_len {
                let len = data.len().min((config_len - rel) as usize);
                self.device.read_config(rel, &mut data[..len]);
            }
            return;
        }
        let mut bytes = [0u8; 24];
        bytes[..4].copy_from_slice(&(self.offered_features() as u32).to_le_bytes());
        bytes[4..8].copy_from_slice(&(self.driver_features as u32).to_le_bytes());
        let sel = self.queue_select as usize;
        let cfg = self.queue_cfg.get(sel).copied().unwrap_or_default();
        let pfn = if cfg.enabled {
            (cfg.desc >> 12) as u32
        } else {
            0
        };
        bytes[8..12].copy_from_slice(&pfn.to_le_bytes());
        let size = self.device.queue_max_sizes().get(sel).copied().unwrap_or(0);
        bytes[12..14].copy_from_slice(&size.to_le_bytes());
        bytes[14..16].copy_from_slice(&self.queue_select.to_le_bytes());
        bytes[18] = self.driver_status;
        let mut int = self.int.lock();
        if offset <= 19 && offset + data.len() as u64 > 19 {
            bytes[19] = int.read_isr();
        }
        bytes[20..22].copy_from_slice(&int.config_vector.to_le_bytes());
        bytes[22..24].copy_from_slice(
            &int.queue_vectors
                .get(sel)
                .copied()
                .unwrap_or(NO_VECTOR)
                .to_le_bytes(),
        );
        copy_clamped(&bytes[..config_off as usize], offset as usize, data);
    }

    pub(super) fn legacy_write(&mut self, offset: u64, data: &[u8]) {
        let config_off = if self.int.lock().msix_enabled { 24 } else { 20 };
        if offset >= config_off {
            let rel = offset - config_off;
            let config_len = self.legacy_config_len();
            if rel < config_len {
                let len = data.len().min((config_len - rel) as usize);
                self.device.write_config(rel, &data[..len]);
            }
            return;
        }
        match (offset, data.len()) {
            (4, 4) if !self.activated => {
                self.driver_features = u32::from_le_bytes(data.try_into().unwrap()) as u64;
                self.transitional.as_mut().unwrap().active = true;
            }
            (8, 4) if !self.activated => {
                if let Some(cfg) = self.queue_cfg.get_mut(self.queue_select as usize) {
                    let pfn = u32::from_le_bytes(data.try_into().unwrap());
                    let size = self.device.queue_max_sizes()[self.queue_select as usize];
                    let desc = (pfn as u64) << 12;
                    let avail = desc + 16 * size as u64;
                    // Legacy layout includes used_event even when EVENT_IDX
                    // was not negotiated; the used ring is page-aligned.
                    let used = (avail + 6 + 2 * size as u64).next_multiple_of(4096);
                    *cfg = QueueCfg {
                        size,
                        desc,
                        avail,
                        used,
                        enabled: pfn != 0,
                    };
                    self.transitional.as_mut().unwrap().active = true;
                }
            }
            (14, 2) => self.queue_select = u16::from_le_bytes(data.try_into().unwrap()),
            (16, 2) if self.activated => {
                let queue = u16::from_le_bytes(data.try_into().unwrap());
                if (queue as usize) < self.num_queues() {
                    self.device.queue_notify(queue);
                }
            }
            (18, 1) => {
                self.transitional.as_mut().unwrap().active = true;
                self.status_write(data[0]);
            }
            (20 | 22, 2) if config_off == 24 => {
                let mut int = self.int.lock();
                let vector = u16::from_le_bytes(data.try_into().unwrap());
                let vector = if (vector as usize) < int.entries.len() {
                    vector
                } else {
                    NO_VECTOR
                };
                if offset == 20 {
                    int.config_vector = vector;
                } else if let Some(slot) = int.queue_vectors.get_mut(self.queue_select as usize) {
                    *slot = vector;
                }
            }
            _ => {}
        }
    }

    fn legacy_config_len(&self) -> u64 {
        // Default net config ends after MAC/status. MTU extends it only when
        // advertised, increasing the I/O BAR to the next power of two.
        if self.device.device_type() == 1 && self.device.features() & (1 << 3) == 0 {
            8
        } else {
            self.device_cfg_len
        }
    }
}

fn validate_net_rom(image: &[u8]) -> Result<(), DeviceError> {
    let bad = || DeviceError::Other("invalid 1af4:1000 PCI option ROM".into());
    if image.is_empty() || image.len() > ROM_SIZE {
        return Err(bad());
    }
    let mut offset = 0;
    loop {
        let data = image.get(offset..).ok_or_else(bad)?;
        if data.len() < 0x1a || data[..2] != [0x55, 0xaa] {
            return Err(bad());
        }
        let pcir = u16::from_le_bytes([data[0x18], data[0x19]]) as usize;
        let header = data.get(pcir..pcir + 0x18).ok_or_else(bad)?;
        if header[..8] != [b'P', b'C', b'I', b'R', 0xf4, 0x1a, 0, 0x10] {
            return Err(bad());
        }
        let len = u16::from_le_bytes([header[0x10], header[0x11]]) as usize * 512;
        if len < pcir + 0x18 || len > data.len() || !matches!(header[0x14], 0 | 3) {
            return Err(bad());
        }
        if header[0x14] == 0 && data[..len].iter().fold(0u8, |sum, b| sum.wrapping_add(*b)) != 0 {
            return Err(bad());
        }
        offset += len;
        if header[0x15] & 0x80 != 0 {
            return if offset == image.len() {
                Ok(())
            } else {
                Err(bad())
            };
        }
    }
}

fn bar_value(value: u64, size: u64, flags: u32, mask: u32) -> u32 {
    let value = value as u32;
    if value & mask == mask {
        (!(size - 1) as u32 & mask) | flags
    } else {
        (value & !(size - 1) as u32 & mask) | flags
    }
}

fn set_byte32(value: &mut u32, index: usize, byte: u8) {
    let shift = index * 8;
    *value = (*value & !(0xff << shift)) | ((byte as u32) << shift);
}

fn set_byte16(value: &mut u16, index: usize, byte: u8) {
    let shift = index * 8;
    *value = (*value & !(0xff << shift)) | ((byte as u16) << shift);
}
