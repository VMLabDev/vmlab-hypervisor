//! PCI assignment policy. Host VFIO and KVM operations live behind `Backend`.

use crate::{BarInfo, BarKind, MsiMessage, PciDevice};

pub trait Backend: Send {
    fn read(&self, region: u8, offset: u64, data: &mut [u8]);
    fn write(&self, region: u8, offset: u64, data: &[u8]);
    fn map(&mut self, bar: u8, base: Option<u64>, traps: &[(u64, u64)]) -> Result<(), String>;
    fn interrupts(
        &mut self,
        mode: InterruptMode,
        messages: &[Option<MsiMessage>],
    ) -> Result<(), String>;
    fn pending(&self, mode: InterruptMode, vector: usize) -> bool;
    fn reset(&mut self) -> Result<(), String>;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InterruptMode {
    Disabled,
    Intx,
    Msi,
    Msix,
}

#[derive(Clone, Copy)]
struct Msix {
    cap: usize,
    table_bar: u8,
    table: u64,
    pba_bar: u8,
    pba: u64,
    count: usize,
}

pub struct VfioPci<B: Backend> {
    backend: B,
    config: Vec<u8>,
    initial: Vec<u8>,
    host_config: Vec<bool>,
    bars: Vec<BarInfo>,
    msi: Option<usize>,
    msix: Option<Msix>,
    table: Vec<u8>,
    addresses: [Option<u64>; 6],
    error: Option<String>,
}

fn word(bytes: &[u8], offset: usize) -> u16 {
    u16::from_le_bytes(bytes[offset..offset + 2].try_into().unwrap())
}

fn dword(bytes: &[u8], offset: usize) -> u32 {
    u32::from_le_bytes(bytes[offset..offset + 4].try_into().unwrap())
}

impl<B: Backend> VfioPci<B> {
    pub fn new(mut backend: B, bars: Vec<BarInfo>, mut config: Vec<u8>) -> Result<Self, String> {
        if !matches!(config.len(), 256 | 4096) || config[14] & 0x7f != 0 {
            return Err("VFIO requires a PCI endpoint with 256 or 4096 config bytes".into());
        }
        backend.reset()?;
        backend.write(7, 4, &[0, 0]);
        config[4..6].fill(0);
        config[0x30..0x34].fill(0);
        for reg in (0x10..0x28).step_by(4) {
            let flags = config[reg] & 15;
            config[reg..reg + 4].fill(0);
            config[reg] = flags;
        }
        let mut msi = None;
        let mut msix = None;
        let mut host_config = vec![true; config.len()];
        host_config[..6].fill(false);
        host_config[0x0e] = false;
        host_config[0x10..0x28].fill(false);
        host_config[0x30..0x34].fill(false);
        host_config[0x3c..0x3e].fill(false);
        let mut cap = (config[0x34] & !3) as usize;
        let mut seen = [false; 256];
        while cap != 0 {
            if !(0x40..=0xfc).contains(&cap) || seen[cap] {
                return Err("invalid VFIO PCI capability chain".into());
            }
            seen[cap] = true;
            match config[cap] {
                5 => {
                    let control = word(&config, cap + 2);
                    let len = if control & 0x100 != 0 {
                        if control & 0x80 != 0 { 24 } else { 20 }
                    } else if control & 0x80 != 0 {
                        14
                    } else {
                        10
                    };
                    if cap + len > 256 || msi.is_some() {
                        return Err("invalid MSI capability".into());
                    }
                    msi = Some(cap);
                    // Advertise a single message; preserve 64-bit and mask support.
                    config[cap + 2] &= 0x80;
                    config[cap + 3] &= 1;
                    config[cap + 4..cap + len].fill(0);
                    host_config[cap..cap + len].fill(false);
                }
                0x11 => {
                    if cap + 12 > 256 || msix.is_some() {
                        return Err("invalid MSI-X capability".into());
                    }
                    let table = dword(&config, cap + 4);
                    let pba = dword(&config, cap + 8);
                    let count = usize::from(word(&config, cap + 2) & 0x7ff) + 1;
                    let x = Msix {
                        cap,
                        table_bar: (table & 7) as u8,
                        table: u64::from(table & !7),
                        pba_bar: (pba & 7) as u8,
                        pba: u64::from(pba & !7),
                        count,
                    };
                    for (index, offset, size) in [
                        (x.table_bar, x.table, count as u64 * 16),
                        (x.pba_bar, x.pba, (count as u64).div_ceil(64) * 8),
                    ] {
                        if index >= 6
                            || !bars.iter().any(|b| {
                                b.index == index
                                    && !matches!(b.kind, BarKind::Io)
                                    && offset.checked_add(size).is_some_and(|end| end <= b.size)
                            })
                        {
                            return Err("MSI-X structure lies outside a memory BAR".into());
                        }
                    }
                    config[cap + 3] &= 0x3f;
                    host_config[cap..cap + 12].fill(false);
                    msix = Some(x);
                }
                _ => {}
            }
            cap = (config[cap + 1] & !3) as usize;
        }
        let mut table = vec![0; msix.map_or(0, |x| x.count * 16)];
        for entry in table.as_chunks_mut::<16>().0 {
            entry[12] = 1;
        }
        let initial = config.clone();
        let mut device = Self {
            backend,
            config,
            initial,
            host_config,
            bars,
            msi,
            msix,
            table,
            addresses: [None; 6],
            error: None,
        };
        device.update_interrupts()?;
        Ok(device)
    }

    pub fn reset(&mut self) -> Result<(), String> {
        self.backend.interrupts(InterruptMode::Disabled, &[])?;
        self.backend.reset()?;
        self.backend.write(7, 4, &[0, 0]);
        self.config.clone_from(&self.initial);
        self.table.fill(0);
        for entry in self.table.as_chunks_mut::<16>().0 {
            entry[12] = 1;
        }
        self.addresses.fill(None);
        self.update_maps()?;
        self.update_interrupts()
    }

    fn traps(&self, bar: u8) -> Vec<(u64, u64)> {
        let mut traps = Vec::new();
        if let Some(x) = self.msix {
            if bar == x.table_bar {
                traps.push((x.table, x.count as u64 * 16));
            }
            if bar == x.pba_bar {
                traps.push((x.pba, (x.count as u64).div_ceil(64) * 8));
            }
        }
        traps
    }

    fn update_maps(&mut self) -> Result<(), String> {
        for bar in self.bars.clone().into_iter().filter(|b| b.index < 6) {
            let enabled = self.config[4]
                & if matches!(bar.kind, BarKind::Io) {
                    1
                } else {
                    2
                }
                != 0;
            self.backend.map(
                bar.index,
                self.addresses[bar.index as usize].filter(|_| enabled),
                &self.traps(bar.index),
            )?;
        }
        Ok(())
    }

    fn update_interrupts(&mut self) -> Result<(), String> {
        if let Some(x) = self.msix
            && self.config[x.cap + 3] & 0x80 != 0
        {
            let mask = self.config[x.cap + 3] & 0x40 != 0;
            let messages: Vec<_> = self
                .table
                .as_chunks::<16>()
                .0
                .iter()
                .map(|entry| {
                    (!mask && entry[12] & 1 == 0).then(|| MsiMessage {
                        address: u64::from_le_bytes(entry[..8].try_into().unwrap()),
                        data: dword(entry, 8),
                    })
                })
                .collect();
            return self.backend.interrupts(InterruptMode::Msix, &messages);
        }
        if let Some(cap) = self.msi
            && self.config[cap + 2] & 1 != 0
        {
            let wide = self.config[cap + 2] & 0x80 != 0;
            let data_offset = cap + if wide { 12 } else { 8 };
            let mask_offset = cap + if wide { 16 } else { 12 };
            let masked = self.config[cap + 3] & 1 != 0 && self.config[mask_offset] & 1 != 0;
            let address = u64::from(dword(&self.config, cap + 4))
                | if wide {
                    u64::from(dword(&self.config, cap + 8)) << 32
                } else {
                    0
                };
            return self.backend.interrupts(
                InterruptMode::Msi,
                &[(!masked).then_some(MsiMessage {
                    address,
                    data: u32::from(word(&self.config, data_offset)),
                })],
            );
        }
        self.backend.interrupts(
            if word(&self.config, 4) & 0x400 == 0 && self.config[0x3d] != 0 {
                InterruptMode::Intx
            } else {
                InterruptMode::Disabled
            },
            &[],
        )
    }

    fn latch(&mut self, result: Result<(), String>) {
        if let Err(error) = result {
            self.error = Some(error);
            self.backend.write(7, 4, &[0, 0]);
        }
    }
}

impl<B: Backend> PciDevice for VfioPci<B> {
    fn config_read(&mut self, reg: usize) -> u32 {
        let offset = reg.saturating_mul(4);
        if offset
            .checked_add(4)
            .is_none_or(|end| end > self.config.len())
        {
            return u32::MAX;
        }
        if let Some(cap) = self.msi {
            let pending = cap
                + if self.config[cap + 2] & 0x80 != 0 {
                    20
                } else {
                    16
                };
            if self.config[cap + 3] & 1 != 0 && offset == pending {
                return u32::from(self.backend.pending(InterruptMode::Msi, 0));
            }
        }
        let mut value: [u8; 4] = self.config[offset..offset + 4].try_into().unwrap();
        if self.host_config[offset..offset + 4]
            .iter()
            .any(|owned| *owned)
        {
            let mut host = [0; 4];
            self.backend.read(7, offset as u64, &mut host);
            for i in 0..4 {
                if self.host_config[offset + i] {
                    value[i] = host[i];
                }
            }
        }
        u32::from_le_bytes(value)
    }

    fn config_write(&mut self, reg: usize, offset: u64, data: &[u8]) {
        let start = reg.saturating_mul(4).saturating_add(offset as usize);
        if start
            .checked_add(data.len())
            .is_none_or(|end| end > self.config.len())
        {
            return;
        }
        for (i, &byte) in data.iter().enumerate() {
            let pos = start + i;
            if (0x10..0x28).contains(&pos) || (0x30..0x34).contains(&pos) {
                continue;
            }
            if pos == 4 || pos == 5 {
                let mask = if pos == 4 { 7 } else { 4 };
                self.config[pos] = byte & mask;
                let command = word(&self.config, 4) & !0x400;
                self.backend.write(7, 4, &command.to_le_bytes());
                continue;
            }
            if let Some(cap) = self.msi {
                let wide = self.config[cap + 2] & 0x80 != 0;
                let end = cap
                    + if self.config[cap + 3] & 1 != 0 {
                        if wide { 24 } else { 20 }
                    } else if wide {
                        14
                    } else {
                        10
                    };
                if (cap..end).contains(&pos) {
                    let data_end = cap + if wide { 14 } else { 10 };
                    let mask = cap + if wide { 16 } else { 12 };
                    if pos == cap + 2 {
                        self.config[pos] = (self.config[pos] & !1) | (byte & 1);
                    } else if (cap + 4..data_end).contains(&pos) {
                        self.config[pos] = byte;
                    } else if self.config[cap + 3] & 1 != 0 && pos == mask {
                        self.config[pos] = byte & 1;
                    }
                    continue;
                }
            }
            if let Some(x) = self.msix
                && (x.cap..x.cap + 12).contains(&pos)
            {
                if pos == x.cap + 3 {
                    self.config[pos] = (self.config[pos] & 0x3f) | (byte & 0xc0);
                }
                continue;
            }
            // Header fields and capabilities not owned by the VMM use the
            // kernel VFIO config-space permissions below.
            if pos == 0x3c {
                self.config[pos] = byte;
            }
        }
        let mut i = 0;
        while i < data.len() {
            if !self.host_config[start + i] {
                i += 1;
                continue;
            }
            let first = i;
            while i < data.len() && self.host_config[start + i] {
                i += 1;
            }
            self.backend
                .write(7, (start + first) as u64, &data[first..i]);
        }
        let result = self.update_maps().and_then(|()| self.update_interrupts());
        self.latch(result);
    }

    fn bars(&self) -> Vec<BarInfo> {
        self.bars.clone()
    }

    fn set_bar_address(&mut self, bar: u8, address: Option<u64>) {
        if bar < 6 {
            self.addresses[bar as usize] = address;
            let result = self.update_maps();
            self.latch(result);
        }
    }

    fn take_error(&mut self) -> Option<String> {
        self.error.take()
    }

    fn bar_read(&mut self, bar: u8, offset: u64, data: &mut [u8]) {
        if !self.traps(bar).iter().any(|&(start, size)| {
            offset < start + size && start < offset.saturating_add(data.len() as u64)
        }) {
            self.backend.read(bar, offset, data);
            return;
        }
        for (i, byte) in data.iter_mut().enumerate() {
            let at = offset + i as u64;
            if let Some(x) = self.msix {
                if bar == x.table_bar && at >= x.table && at - x.table < self.table.len() as u64 {
                    *byte = self.table[(at - x.table) as usize];
                    continue;
                }
                if bar == x.pba_bar && at >= x.pba && at - x.pba < (x.count as u64).div_ceil(64) * 8
                {
                    let first = (at - x.pba) as usize * 8;
                    *byte = (0..8).fold(0, |bits, b| {
                        bits | (u8::from(
                            first + b < x.count
                                && self.backend.pending(InterruptMode::Msix, first + b),
                        ) << b)
                    });
                    continue;
                }
            }
            self.backend.read(bar, at, std::slice::from_mut(byte));
        }
    }

    fn bar_write(&mut self, bar: u8, offset: u64, data: &[u8]) {
        if !self.traps(bar).iter().any(|&(start, size)| {
            offset < start + size && start < offset.saturating_add(data.len() as u64)
        }) {
            self.backend.write(bar, offset, data);
            return;
        }
        let mut changed = false;
        for (i, &byte) in data.iter().enumerate() {
            let at = offset + i as u64;
            if let Some(x) = self.msix {
                if bar == x.table_bar && at >= x.table && at - x.table < self.table.len() as u64 {
                    let index = (at - x.table) as usize;
                    self.table[index] = if index % 16 == 12 {
                        byte & 1
                    } else if index % 16 > 12 {
                        0
                    } else {
                        byte
                    };
                    changed = true;
                    continue;
                }
                if bar == x.pba_bar && at >= x.pba && at - x.pba < (x.count as u64).div_ceil(64) * 8
                {
                    continue;
                }
            }
            self.backend.write(bar, at, &[byte]);
        }
        if changed {
            let result = self.update_interrupts();
            self.latch(result);
        }
    }
}

/// Page-aligned directly mapped spans. Pages containing either MSI-X structure
/// are excluded in their entirety; their other registers remain trapped too.
pub fn mapping_plan(
    size: u64,
    areas: &[(u64, u64)],
    traps: &[(u64, u64)],
    page: u64,
) -> Result<Vec<(u64, u64)>, String> {
    if !page.is_power_of_two() {
        return Err("invalid host page size".into());
    }
    let mut result = Vec::new();
    for &(start, length) in areas {
        if length == 0 {
            continue;
        }
        let end = start
            .checked_add(length)
            .filter(|&end| end <= size)
            .ok_or("invalid VFIO mmap area")?;
        if (start | length) & (page - 1) != 0 {
            return Err("unaligned VFIO mmap area".into());
        }
        let mut spans = vec![(start, end)];
        for &(offset, length) in traps {
            let a = offset & !(page - 1);
            let b = offset
                .checked_add(length)
                .and_then(|v| v.checked_add(page - 1))
                .ok_or("invalid trap range")?
                & !(page - 1);
            spans = spans
                .into_iter()
                .flat_map(|(lo, hi)| {
                    if b <= lo || a >= hi {
                        vec![(lo, hi)]
                    } else {
                        [(lo, a.min(hi)), (b.max(lo), hi)]
                            .into_iter()
                            .filter(|(l, h)| l < h)
                            .collect()
                    }
                })
                .collect();
        }
        result.extend(spans.into_iter().map(|(lo, hi)| (lo, hi - lo)));
    }
    result.sort_unstable();
    if result.windows(2).any(|w| w[0].0 + w[0].1 > w[1].0) {
        return Err("overlapping VFIO mmap areas".into());
    }
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    type MapRequest = (u8, Option<u64>, Vec<(u64, u64)>);

    #[derive(Default)]
    struct State {
        writes: Vec<(u8, u64, Vec<u8>)>,
        maps: Vec<MapRequest>,
        interrupts: Vec<(InterruptMode, Vec<Option<MsiMessage>>)>,
        resets: usize,
        fail: bool,
    }
    #[derive(Clone, Default)]
    struct Mock(Arc<Mutex<State>>);
    impl Backend for Mock {
        fn read(&self, _: u8, _: u64, data: &mut [u8]) {
            data.fill(0xa5);
        }
        fn write(&self, bar: u8, offset: u64, data: &[u8]) {
            self.0
                .lock()
                .unwrap()
                .writes
                .push((bar, offset, data.to_vec()));
        }
        fn map(&mut self, bar: u8, base: Option<u64>, traps: &[(u64, u64)]) -> Result<(), String> {
            let mut state = self.0.lock().unwrap();
            state.maps.push((bar, base, traps.to_vec()));
            if state.fail {
                Err("mapping failed".into())
            } else {
                Ok(())
            }
        }
        fn interrupts(
            &mut self,
            mode: InterruptMode,
            messages: &[Option<MsiMessage>],
        ) -> Result<(), String> {
            self.0
                .lock()
                .unwrap()
                .interrupts
                .push((mode, messages.to_vec()));
            Ok(())
        }
        fn pending(&self, _: InterruptMode, vector: usize) -> bool {
            vector == 1
        }
        fn reset(&mut self) -> Result<(), String> {
            self.0.lock().unwrap().resets += 1;
            Ok(())
        }
    }

    fn configuration() -> Vec<u8> {
        let mut config = vec![0; 4096];
        config[..4].copy_from_slice(&0x1234_8086u32.to_le_bytes());
        config[0x10..0x14].copy_from_slice(&0xabcd_0004u32.to_le_bytes());
        config[0x34] = 0x40;
        config[0x3d] = 1;
        config[0x40] = 5;
        config[0x41] = 0x60;
        config[0x42] = 0x8e;
        config[0x43] = 1;
        config[0x60] = 0x11;
        config[0x62] = 1;
        config[0x64..0x68].copy_from_slice(&0x1800u32.to_le_bytes());
        config[0x68..0x6c].copy_from_slice(&0x3000u32.to_le_bytes());
        config
    }

    fn fixture() -> (VfioPci<Mock>, Mock) {
        let mock = Mock::default();
        let device = VfioPci::new(
            mock.clone(),
            vec![BarInfo {
                index: 0,
                size: 0x8000,
                kind: BarKind::Mmio64 {
                    prefetchable: false,
                },
            }],
            configuration(),
        )
        .unwrap();
        mock.0.lock().unwrap().writes.clear();
        (device, mock)
    }

    fn write(device: &mut VfioPci<Mock>, offset: usize, bytes: &[u8]) {
        device.config_write(offset / 4, (offset % 4) as u64, bytes);
    }

    #[test]
    fn config_virtualization_and_command_decoding() {
        let (mut device, mock) = fixture();
        assert_eq!(device.config_read(0), 0x1234_8086);
        assert_eq!(device.config_read(4), 4);
        assert_eq!(device.config_read(0x30 / 4), 0);
        write(&mut device, 0, &[0; 4]);
        write(&mut device, 0x10, &[0xff; 4]);
        assert_eq!(device.config_read(0), 0x1234_8086);
        assert_eq!(device.config_read(4), 4);
        device.set_bar_address(0, Some(1 << 32));
        write(&mut device, 4, &0xffffu16.to_le_bytes());
        assert_eq!(device.config_read(1) & 0xffff, 0x407);
        let state = mock.0.lock().unwrap();
        assert_eq!(state.writes.last().unwrap(), &(7, 4, vec![7, 0]));
        assert_eq!(
            state.maps.last().unwrap(),
            &(0, Some(1 << 32), vec![(0x1800, 32), (0x3000, 8)])
        );
        drop(state);
        write(&mut device, 4, &[0, 0]);
        assert_eq!(mock.0.lock().unwrap().maps.last().unwrap().1, None);
    }

    #[test]
    fn msix_masks_pending_bits_and_table_never_reach_hardware() {
        let (mut device, mock) = fixture();
        device.bar_write(0, 0x1810, &0xfee0_0000u64.to_le_bytes());
        device.bar_write(0, 0x1818, &0x44u32.to_le_bytes());
        write(&mut device, 0x63, &[0x80]);
        assert_eq!(
            mock.0.lock().unwrap().interrupts.last().unwrap().1,
            vec![None, None]
        );
        device.bar_write(0, 0x181c, &0u32.to_le_bytes());
        assert_eq!(
            mock.0.lock().unwrap().interrupts.last().unwrap().1[1],
            Some(MsiMessage {
                address: 0xfee0_0000,
                data: 0x44
            })
        );
        write(&mut device, 0x63, &[0xc0]);
        assert_eq!(
            mock.0.lock().unwrap().interrupts.last().unwrap().1,
            vec![None, None]
        );
        let mut pba = [0; 8];
        device.bar_read(0, 0x3000, &mut pba);
        assert_eq!(pba, [2, 0, 0, 0, 0, 0, 0, 0]);
        device.bar_write(0, 0x3000, &[0xff; 8]);
        assert!(mock.0.lock().unwrap().writes.is_empty());
        let mut boundary = [0; 4];
        device.bar_read(0, 0x17fe, &mut boundary);
        assert_eq!(boundary, [0xa5, 0xa5, 0, 0]);
    }

    #[test]
    fn msi_is_single_vector_maskable_and_intx_is_suppressed() {
        let (mut device, mock) = fixture();
        assert_eq!(device.config_read(0x40 / 4) >> 16, 0x180);
        write(&mut device, 0x44, &0xfee0_0000u32.to_le_bytes());
        write(&mut device, 0x4c, &0x51u16.to_le_bytes());
        write(&mut device, 0x42, &[0xff]);
        let state = mock.0.lock().unwrap();
        assert_eq!(
            state.interrupts.last().unwrap(),
            &(
                InterruptMode::Msi,
                vec![Some(MsiMessage {
                    address: 0xfee0_0000,
                    data: 0x51
                })]
            )
        );
        drop(state);
        write(&mut device, 0x50, &[1]);
        assert_eq!(
            mock.0.lock().unwrap().interrupts.last().unwrap(),
            &(InterruptMode::Msi, vec![None])
        );
        write(&mut device, 0x42, &[0]);
        assert_eq!(
            mock.0.lock().unwrap().interrupts.last().unwrap().0,
            InterruptMode::Intx
        );
        write(&mut device, 5, &[4]);
        assert_eq!(
            mock.0.lock().unwrap().interrupts.last().unwrap().0,
            InterruptMode::Disabled
        );
    }

    #[test]
    fn ordinary_bar_access_preserves_width_and_reset_clears_state() {
        let (mut device, mock) = fixture();
        device.bar_write(0, 0, &[1, 2, 3, 4]);
        assert_eq!(
            mock.0.lock().unwrap().writes.as_slice(),
            &[(0, 0, vec![1, 2, 3, 4])]
        );
        device.bar_write(0, 0x180c, &[0]);
        device.reset().unwrap();
        let mut mask = [0];
        device.bar_read(0, 0x180c, &mut mask);
        assert_eq!(mask, [1]);
        assert_eq!(mock.0.lock().unwrap().resets, 2);
        mock.0.lock().unwrap().fail = true;
        write(&mut device, 4, &[7]);
        assert_eq!(device.take_error().as_deref(), Some("mapping failed"));
        assert_eq!(
            mock.0.lock().unwrap().writes.last().unwrap(),
            &(7, 4, vec![0, 0])
        );
    }

    #[test]
    fn non_interrupt_capabilities_use_live_vfio_config_access() {
        let (mut device, mock) = fixture();
        assert_eq!(device.config_read(0x100 / 4), 0xa5a5_a5a5);
        write(&mut device, 0x100, &[1, 2, 3, 4]);
        assert_eq!(
            mock.0.lock().unwrap().writes.last().unwrap(),
            &(7, 0x100, vec![1, 2, 3, 4])
        );
    }

    #[test]
    fn page_plan_excludes_both_structures_and_honours_sparse_mmap() {
        assert_eq!(
            mapping_plan(0x8000, &[(0, 0x8000)], &[(0x1800, 32), (0x3000, 8)], 4096).unwrap(),
            vec![(0, 0x1000), (0x2000, 0x1000), (0x4000, 0x4000)]
        );
        assert_eq!(
            mapping_plan(0x8000, &[(0, 4096), (0x4000, 4096)], &[], 4096).unwrap(),
            vec![(0, 4096), (0x4000, 4096)]
        );
        assert!(mapping_plan(4096, &[(0, 8192)], &[], 4096).is_err());
        assert!(mapping_plan(8192, &[(0, 8192), (4096, 4096)], &[], 4096).is_err());
    }

    #[test]
    fn malformed_capability_chains_and_msix_ranges_are_rejected() {
        let mut config = configuration();
        config[0x61] = 0x40;
        assert!(VfioPci::new(Mock::default(), vec![], config).is_err());
        assert!(VfioPci::new(Mock::default(), vec![], configuration()).is_err());
    }
}
