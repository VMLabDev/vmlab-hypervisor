//! Interrupt controllers for the captured RISC-V virt platform.

use serde::{Deserialize, Serialize};

use crate::{BusDevice, DeviceError, Snapshot};
pub mod rtc;
pub mod virtio_mmio;

pub const TIMEBASE_HZ: u64 = 10_000_000;
pub const PLIC_SOURCES: usize = 96;

/// SiFive CLINT layout: MSIP, MTIMECMP, and MTIME. Time is supplied by the
/// machine clock so pausing a VM also pauses its timer.
#[derive(Clone, Serialize, Deserialize)]
pub struct Clint {
    msip: Vec<u32>,
    mtimecmp: Vec<u64>,
    mtime: u64,
}

impl Clint {
    pub fn new(harts: usize) -> Self {
        assert!((1..=4095).contains(&harts));
        Self {
            msip: vec![0; harts],
            mtimecmp: vec![u64::MAX; harts],
            mtime: 0,
        }
    }

    pub fn time(&self) -> u64 {
        self.mtime
    }

    pub fn advance(&mut self, ticks: u64) {
        self.mtime = self.mtime.wrapping_add(ticks);
    }

    pub fn compare(&self, hart: usize) -> u64 {
        self.mtimecmp[hart]
    }

    /// Machine software and timer pending bits, in MIP positions.
    pub fn pending(&self, hart: usize) -> u64 {
        (u64::from(self.msip[hart] != 0) << 3) | (u64::from(self.mtime >= self.mtimecmp[hart]) << 7)
    }

    fn register(&self, offset: u64) -> Option<(u64, u64, usize)> {
        if offset < self.msip.len() as u64 * 4 {
            let hart = offset as usize / 4;
            Some((hart as u64 * 4, self.msip[hart] as u64, 4))
        } else if offset >= 0x4000 && offset < 0x4000 + self.mtimecmp.len() as u64 * 8 {
            let hart = (offset as usize - 0x4000) / 8;
            Some((0x4000 + hart as u64 * 8, self.mtimecmp[hart], 8))
        } else if (0xbff8..0xc000).contains(&offset) {
            Some((0xbff8, self.mtime, 8))
        } else {
            None
        }
    }
}

impl BusDevice for Clint {
    fn read(&mut self, offset: u64, data: &mut [u8]) {
        data.fill(0);
        if let Some((base, value, width)) = self.register(offset) {
            let start = (offset - base) as usize;
            if start + data.len() <= width {
                data.copy_from_slice(&value.to_le_bytes()[start..start + data.len()]);
            }
        }
    }

    fn write(&mut self, offset: u64, data: &[u8]) {
        if let Some((base, value, width)) = self.register(offset) {
            let start = (offset - base) as usize;
            if start + data.len() > width {
                return;
            }
            let mut bytes = value.to_le_bytes();
            bytes[start..start + data.len()].copy_from_slice(data);
            let value = u64::from_le_bytes(bytes);
            if base < 0x4000 {
                self.msip[base as usize / 4] = value as u32 & 1;
            } else if base < 0xbff8 {
                self.mtimecmp[(base as usize - 0x4000) / 8] = value;
            } else {
                self.mtime = value;
            }
        }
    }
}

/// PLIC with one M and one S context per hart. Each source has one gateway;
/// deassertion does not retract a request already latched in the core.
#[derive(Clone, Serialize, Deserialize)]
pub struct Plic {
    priorities: Vec<u32>,
    levels: Vec<bool>,
    pending: Vec<bool>,
    busy: Vec<bool>,
    enables: Vec<[u32; 3]>,
    thresholds: Vec<u32>,
}

/// Wired-OR inputs for devices sharing a PLIC source, including PCI INTx.
pub struct PlicIrqs {
    controller: std::sync::Arc<parking_lot::Mutex<Plic>>,
    levels: parking_lot::Mutex<Vec<u32>>,
}

impl PlicIrqs {
    pub fn new(controller: std::sync::Arc<parking_lot::Mutex<Plic>>) -> std::sync::Arc<Self> {
        std::sync::Arc::new(Self {
            controller,
            levels: parking_lot::Mutex::new(vec![0; PLIC_SOURCES]),
        })
    }
    pub fn line(self: &std::sync::Arc<Self>, source: usize) -> std::sync::Arc<dyn crate::IrqLine> {
        assert!((1..PLIC_SOURCES).contains(&source));
        std::sync::Arc::new(PlicInput {
            hub: self.clone(),
            source,
            high: std::sync::atomic::AtomicBool::new(false),
        })
    }
}

struct PlicInput {
    hub: std::sync::Arc<PlicIrqs>,
    source: usize,
    high: std::sync::atomic::AtomicBool,
}

impl crate::IrqLine for PlicInput {
    fn set_level(&self, high: bool) {
        let mut levels = self.hub.levels.lock();
        if self.high.swap(high, std::sync::atomic::Ordering::SeqCst) == high {
            return;
        }
        if high {
            levels[self.source] += 1;
        } else {
            levels[self.source] -= 1;
        }
        self.hub
            .controller
            .lock()
            .set_level(self.source, levels[self.source] != 0);
    }
}

impl Drop for PlicInput {
    fn drop(&mut self) {
        crate::IrqLine::set_level(self, false);
    }
}

impl Plic {
    pub fn new(harts: usize) -> Self {
        assert!((1..=512).contains(&harts));
        Self {
            priorities: vec![0; PLIC_SOURCES],
            levels: vec![false; PLIC_SOURCES],
            pending: vec![false; PLIC_SOURCES],
            busy: vec![false; PLIC_SOURCES],
            enables: vec![[0; 3]; harts * 2],
            thresholds: vec![0; harts * 2],
        }
    }

    pub fn set_level(&mut self, source: usize, high: bool) {
        if source == 0 || source >= PLIC_SOURCES {
            return;
        }
        self.levels[source] = high;
        if high && !self.busy[source] {
            self.busy[source] = true;
            self.pending[source] = true;
        }
    }

    fn enabled(&self, context: usize, source: usize) -> bool {
        self.enables[context][source / 32] & (1 << (source % 32)) != 0
    }

    fn best(&self, context: usize) -> usize {
        let mut best = 0;
        for source in 1..PLIC_SOURCES {
            if self.pending[source]
                && self.enabled(context, source)
                && self.priorities[source] > self.priorities[best]
            {
                best = source;
            }
        }
        best
    }

    pub fn asserted(&self, context: usize) -> bool {
        let source = self.best(context);
        source != 0 && self.priorities[source] > self.thresholds[context]
    }

    pub fn pending(&self, hart: usize) -> u64 {
        (u64::from(self.asserted(hart * 2)) << 11) | (u64::from(self.asserted(hart * 2 + 1)) << 9)
    }

    fn claim(&mut self, context: usize) -> u32 {
        let source = self.best(context);
        self.pending[source] = false;
        source as u32
    }

    fn complete(&mut self, context: usize, source: usize) {
        if source > 0 && source < PLIC_SOURCES && self.enabled(context, source) {
            self.busy[source] = self.levels[source];
            self.pending[source] |= self.levels[source];
        }
    }

    fn context(&self, offset: u64) -> Option<(usize, u64)> {
        let offset = offset.checked_sub(0x20_0000)?;
        let context = (offset / 0x1000) as usize;
        (context < self.enables.len()).then_some((context, offset % 0x1000))
    }
}

impl BusDevice for Plic {
    fn read(&mut self, offset: u64, data: &mut [u8]) {
        data.fill(0);
        if data.len() != 4 || offset & 3 != 0 {
            return;
        }
        let value = if offset < PLIC_SOURCES as u64 * 4 {
            self.priorities[offset as usize / 4]
        } else if (0x1000..0x100c).contains(&offset) {
            let word = (offset as usize - 0x1000) / 4;
            (0..32).fold(0, |bits, bit| {
                bits | (u32::from(self.pending[word * 32 + bit]) << bit)
            })
        } else if offset >= 0x2000 && offset < 0x2000 + self.enables.len() as u64 * 0x80 {
            let context = (offset as usize - 0x2000) / 0x80;
            let word = (offset as usize % 0x80) / 4;
            self.enables[context].get(word).copied().unwrap_or(0)
        } else if let Some((context, register)) = self.context(offset) {
            match register {
                0 => self.thresholds[context],
                4 => self.claim(context),
                _ => 0,
            }
        } else {
            0
        };
        data.copy_from_slice(&value.to_le_bytes());
    }

    fn write(&mut self, offset: u64, data: &[u8]) {
        if data.len() != 4 || offset & 3 != 0 {
            return;
        }
        let value = u32::from_le_bytes(data.try_into().unwrap());
        if offset > 0 && offset < PLIC_SOURCES as u64 * 4 {
            self.priorities[offset as usize / 4] = value & 7;
        } else if offset >= 0x2000 && offset < 0x2000 + self.enables.len() as u64 * 0x80 {
            let context = (offset as usize - 0x2000) / 0x80;
            let word = (offset as usize % 0x80) / 4;
            if let Some(enabled) = self.enables[context].get_mut(word) {
                *enabled = if word == 0 { value & !1 } else { value };
            }
        } else if let Some((context, register)) = self.context(offset) {
            match register {
                0 => self.thresholds[context] = value & 7,
                4 => self.complete(context, value as usize),
                _ => {}
            }
        }
    }
}

macro_rules! snapshot {
    ($device:ty, $id:literal, $valid:expr) => {
        impl Snapshot for $device {
            fn state_id(&self) -> String {
                $id.into()
            }
            fn save_state(&self) -> Result<Vec<u8>, DeviceError> {
                serde_json::to_vec(self).map_err(|e| DeviceError::State(e.to_string()))
            }
            fn restore_state(&mut self, bytes: &[u8]) -> Result<(), DeviceError> {
                let state: Self =
                    serde_json::from_slice(bytes).map_err(|e| DeviceError::State(e.to_string()))?;
                if !($valid)(self, &state) {
                    return Err(DeviceError::State(concat!("incompatible ", $id).into()));
                }
                *self = state;
                Ok(())
            }
        }
    };
}

snapshot!(Clint, "clint", |old: &Clint, new: &Clint| {
    old.msip.len() == new.msip.len()
        && old.mtimecmp.len() == new.mtimecmp.len()
        && new.msip.iter().all(|value| *value <= 1)
});
snapshot!(Plic, "plic", |old: &Plic, new: &Plic| {
    new.priorities.len() == PLIC_SOURCES
        && new.levels.len() == PLIC_SOURCES
        && new.pending.len() == PLIC_SOURCES
        && new.busy.len() == PLIC_SOURCES
        && old.enables.len() == new.enables.len()
        && old.thresholds.len() == new.thresholds.len()
        && new
            .priorities
            .iter()
            .chain(&new.thresholds)
            .all(|v| *v <= 7)
        && new.priorities[0] == 0
        && !new.pending[0]
        && !new.busy[0]
});

#[cfg(test)]
mod tests {
    use super::*;

    fn read(device: &mut dyn BusDevice, offset: u64) -> u32 {
        let mut bytes = [0; 4];
        device.read(offset, &mut bytes);
        u32::from_le_bytes(bytes)
    }

    fn write(device: &mut dyn BusDevice, offset: u64, value: u32) {
        device.write(offset, &value.to_le_bytes());
    }

    #[test]
    fn clint_split_registers_and_independent_harts() {
        let mut clint = Clint::new(2);
        clint.write(0x4008, &10u64.to_le_bytes());
        clint.advance(9);
        assert_eq!(clint.pending(1), 0);
        clint.advance(1);
        assert_eq!(clint.pending(1), 1 << 7);
        assert_eq!(clint.pending(0), 0);
        write(&mut clint, 4, u32::MAX);
        assert_eq!(read(&mut clint, 4), 1);
        assert_eq!(clint.pending(1), (1 << 7) | (1 << 3));
        write(&mut clint, 0x400c, 1);
        assert_eq!(clint.pending(1), 1 << 3);
        assert_eq!(read(&mut clint, 0xbff8), 10);
        let state = clint.save_state().unwrap();
        clint.advance(100);
        clint.restore_state(&state).unwrap();
        assert_eq!(clint.time(), 10);
        assert!(Clint::new(1).restore_state(&state).is_err());
    }

    #[test]
    fn plic_priority_claim_completion_and_level_gateway() {
        let mut plic = Plic::new(2);
        write(&mut plic, 10 * 4, 2);
        write(&mut plic, 11 * 4, 2);
        write(&mut plic, 0x2080, (1 << 10) | (1 << 11));
        plic.set_level(11, true);
        plic.set_level(10, true);
        assert_eq!(plic.pending(0), 1 << 9);
        assert_eq!(plic.pending(1), 0);
        write(&mut plic, 0x201000, 7);
        assert_eq!(plic.pending(0), 0);
        // Claim ignores the threshold and breaks priority ties by source ID.
        assert_eq!(read(&mut plic, 0x201004), 10);
        assert_eq!(read(&mut plic, 0x201004), 11);
        assert_eq!(read(&mut plic, 0x201004), 0);
        write(&mut plic, 0x201004, 10);
        assert_eq!(read(&mut plic, 0x201004), 10);
        plic.set_level(10, false);
        write(&mut plic, 0x201004, 10);
        assert_eq!(read(&mut plic, 0x201004), 0);
        // A latched request survives deassertion before claim.
        plic.set_level(10, true);
        plic.set_level(10, false);
        assert_eq!(read(&mut plic, 0x201004), 10);
    }

    #[test]
    fn plic_snapshot_retains_inflight_and_rejects_wrong_topology() {
        let mut plic = Plic::new(2);
        write(&mut plic, 95 * 4, 7);
        write(&mut plic, 0x2108, 1 << 31);
        plic.set_level(95, true);
        let state = plic.save_state().unwrap();
        assert_eq!(read(&mut plic, 0x202004), 95);
        plic.restore_state(&state).unwrap();
        assert_eq!(read(&mut plic, 0x202004), 95);
        assert!(Plic::new(1).restore_state(&state).is_err());
        write(&mut plic, 0, 7);
        assert_eq!(read(&mut plic, 0), 0);
        write(&mut plic, 0x2100, u32::MAX);
        assert_eq!(read(&mut plic, 0x2100) & 1, 0);
    }

    #[test]
    fn shared_pci_intx_is_wired_or_and_drop_deasserts() {
        let plic = std::sync::Arc::new(parking_lot::Mutex::new(Plic::new(1)));
        let hub = PlicIrqs::new(plic.clone());
        let first = hub.line(33);
        let second = hub.line(33);
        first.set_level(true);
        first.set_level(true);
        second.set_level(true);
        first.set_level(false);
        assert!(plic.lock().levels[33]);
        drop(second);
        assert!(!plic.lock().levels[33]);
        assert_eq!(hub.levels.lock()[33], 0);
    }
}
