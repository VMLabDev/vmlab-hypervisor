//! Intel HPET 1.0a register model with three timers and a 100 MHz clock.

use crate::{BusDevice, DeviceError, IrqLine, Snapshot};
use std::sync::Arc;

const ROUTES: u64 = 0x00ff_0104;
const PERIOD_FS: u64 = 10_000_000;
const TIMER_CAPS: u64 = (ROUTES << 32) | 0x30;

#[derive(Clone, Copy)]
struct Timer {
    config: u64,
    comparator: u64,
    period: u64,
}

/// Map at 0xfed00000 for 0x400 bytes and advance with running guest time.
pub struct Hpet {
    config: u64,
    status: u64,
    counter: u64,
    remainder: u64,
    timers: [Timer; 3],
    irqs: [Arc<dyn IrqLine>; 32],
    legacy: Arc<dyn Fn(bool) + Send + Sync>,
}

impl Hpet {
    /// `legacy` gates PIT and RTC interrupt delivery when replacement is enabled.
    /// IRQ handles must be source-specific inputs to the machine's wired-OR
    /// router, since this model deasserts inactive HPET routes independently.
    /// Array indexes are GSIs: legacy timer 0 uses GSI 2, timer 1 uses GSI 8.
    /// The machine router must also deliver GSI 2 to the PIC's IRQ 0 input.
    pub fn new(irqs: [Arc<dyn IrqLine>; 32], legacy: Arc<dyn Fn(bool) + Send + Sync>) -> Self {
        Self {
            config: 0,
            status: 0,
            counter: 0,
            remainder: 0,
            timers: [Timer {
                config: TIMER_CAPS,
                comparator: u64::MAX,
                period: 0,
            }; 3],
            irqs,
            legacy,
        }
    }

    pub fn legacy_replacement(&self) -> bool {
        self.config & 2 != 0
    }

    /// Invoke from the event loop and before register accesses; pauses freeze it.
    pub fn advance(&mut self, ns: u64) {
        if self.config & 1 == 0 {
            return;
        }
        let elapsed = u128::from(self.remainder) + u128::from(ns);
        let ticks = (elapsed / 10) as u64;
        self.remainder = (elapsed % 10) as u64;
        if ticks == 0 {
            return;
        }
        let old = self.counter;
        self.counter = self.counter.wrapping_add(ticks);
        for index in 0..3 {
            let timer = &mut self.timers[index];
            let mask = if timer.config & (1 << 8) != 0 {
                u64::from(u32::MAX)
            } else {
                u64::MAX
            };
            let distance = timer.comparator.wrapping_sub(old) & mask;
            // A comparator equal to the old count fires on the next wrap.
            let hit = if distance == 0 {
                mask != u64::MAX && ticks > mask
            } else {
                ticks >= distance
            };
            if !hit {
                continue;
            }
            if timer.config & 8 != 0 && timer.period != 0 {
                let period = timer.period & mask;
                if let Some(count) = (ticks - distance).checked_div(period).map(|n| n + 1) {
                    timer.comparator =
                        timer.comparator.wrapping_add(count.wrapping_mul(period)) & mask;
                }
            }
            if timer.config & 4 == 0 {
                continue;
            }
            if timer.config & 2 != 0 {
                self.status |= 1 << index;
            } else {
                let route = self.route(index);
                self.irqs[route].set_level(true);
                self.irqs[route].set_level(false);
            }
        }
        self.update_irqs();
    }

    fn route(&self, index: usize) -> usize {
        if self.legacy_replacement() && index < 2 {
            if index == 0 { 2 } else { 8 }
        } else {
            ((self.timers[index].config >> 9) & 31) as usize
        }
    }

    fn update_irqs(&self) {
        let mut levels = [false; 32];
        for i in 0..3 {
            if self.config & 1 != 0 && self.status & (1 << i) != 0 && self.timers[i].config & 6 == 6
            {
                levels[self.route(i)] = true;
            }
        }
        for (irq, high) in self.irqs.iter().zip(levels) {
            irq.set_level(high);
        }
    }

    fn register(&self, offset: u64) -> u64 {
        match offset {
            0x00 => (PERIOD_FS << 32) | 0x8086_a201,
            0x10 => self.config,
            0x20 => self.status,
            0xf0 => self.counter,
            0x100..=0x15f => {
                let timer = &self.timers[((offset - 0x100) / 0x20) as usize];
                match offset & 0x1f {
                    0 => timer.config,
                    8 => timer.comparator,
                    _ => 0,
                }
            }
            _ => 0,
        }
    }

    fn write_register(&mut self, offset: u64, value: u64, mask: u64) {
        match offset {
            0x10 => {
                self.config = (self.config & !mask | value & mask) & 3;
                (self.legacy)(self.legacy_replacement());
            }
            0x20 => self.status &= !(value & mask & 7),
            0xf0 => self.counter = self.counter & !mask | value & mask,
            0x100..=0x15f => {
                let timer = &mut self.timers[((offset - 0x100) / 0x20) as usize];
                match offset & 0x1f {
                    0 => {
                        let writable = mask & 0x3f4e;
                        timer.config = timer.config & !writable | value & writable;
                        if timer.config & (1 << 8) != 0 {
                            timer.comparator &= u64::from(u32::MAX);
                            timer.period &= u64::from(u32::MAX);
                        }
                    }
                    8 => {
                        let previous = if timer.config & 8 != 0 && timer.config & (1 << 6) == 0 {
                            timer.period
                        } else {
                            timer.comparator
                        };
                        let value = previous & !mask | value & mask;
                        let value = if timer.config & (1 << 8) != 0 {
                            value & u64::from(u32::MAX)
                        } else {
                            value
                        };
                        if timer.config & 8 == 0 || timer.config & (1 << 6) != 0 {
                            timer.comparator = value;
                        }
                        if timer.config & 8 != 0 {
                            timer.period = value;
                        }
                        timer.config &= !(1 << 6);
                    }
                    _ => (),
                }
            }
            _ => (),
        }
        self.update_irqs();
    }
}

impl BusDevice for Hpet {
    fn read(&mut self, offset: u64, data: &mut [u8]) {
        for (i, byte) in data.iter_mut().enumerate() {
            let address = offset.saturating_add(i as u64);
            *byte = if address < 0x400 {
                (self.register(address & !7) >> ((address & 7) * 8)) as u8
            } else {
                0xff
            };
        }
    }
    fn write(&mut self, offset: u64, data: &[u8]) {
        let mut i = 0;
        while i < data.len() {
            let address = offset.saturating_add(i as u64);
            let n = (8 - (address & 7) as usize).min(data.len() - i);
            let mut value = 0;
            let mut mask = 0;
            for j in 0..n {
                let shift = ((address & 7) as usize + j) * 8;
                value |= u64::from(data[i + j]) << shift;
                mask |= 0xffu64 << shift;
            }
            if address < 0x400 {
                self.write_register(address & !7, value, mask);
            }
            i += n;
        }
    }
}

impl Snapshot for Hpet {
    fn state_id(&self) -> String {
        "chipset.hpet".into()
    }
    fn save_state(&self) -> Result<Vec<u8>, DeviceError> {
        let mut state = b"HPT1".to_vec();
        for value in [self.config, self.status, self.counter, self.remainder]
            .into_iter()
            .chain(
                self.timers
                    .iter()
                    .flat_map(|t| [t.config, t.comparator, t.period]),
            )
        {
            state.extend_from_slice(&value.to_le_bytes());
        }
        Ok(state)
    }
    fn restore_state(&mut self, state: &[u8]) -> Result<(), DeviceError> {
        if state.len() != 108 || &state[..4] != b"HPT1" {
            return Err(DeviceError::State("invalid HPET state".into()));
        }
        let values: Vec<u64> = state[4..]
            .as_chunks::<8>()
            .0
            .iter()
            .map(|b| u64::from_le_bytes(*b))
            .collect();
        if values[0] & !3 != 0
            || values[1] & !7 != 0
            || values[3] >= 10
            || (0..3).any(|i| values[4 + i * 3] & !0x3f4e != TIMER_CAPS)
        {
            return Err(DeviceError::State("invalid HPET registers".into()));
        }
        self.config = values[0];
        self.status = values[1];
        self.counter = values[2];
        self.remainder = values[3];
        for i in 0..3 {
            self.timers[i] = Timer {
                config: values[4 + i * 3],
                comparator: values[5 + i * 3],
                period: values[6 + i * 3],
            };
        }
        (self.legacy)(self.legacy_replacement());
        self.update_irqs();
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    #[derive(Default)]
    struct Line {
        high: AtomicBool,
        rises: AtomicUsize,
    }
    impl IrqLine for Line {
        fn set_level(&self, high: bool) {
            if high && !self.high.swap(high, Ordering::SeqCst) {
                self.rises.fetch_add(1, Ordering::SeqCst);
            }
            if !high {
                self.high.store(false, Ordering::SeqCst);
            }
        }
    }
    fn write(hpet: &mut Hpet, offset: u64, value: u64) {
        hpet.write(offset, &value.to_le_bytes());
    }
    fn read(hpet: &mut Hpet, offset: u64) -> u64 {
        let mut data = [0; 8];
        hpet.read(offset, &mut data);
        u64::from_le_bytes(data)
    }
    #[test]
    fn p4_hpet_comparators_level_edge_and_legacy() {
        let lines: [Arc<Line>; 32] = std::array::from_fn(|_| Arc::new(Line::default()));
        let legacy = Arc::new(AtomicBool::new(false));
        let callback = legacy.clone();
        let mut hpet = Hpet::new(
            std::array::from_fn(|i| lines[i].clone() as Arc<dyn IrqLine>),
            Arc::new(move |on| callback.store(on, Ordering::SeqCst)),
        );
        assert_eq!(read(&mut hpet, 0) as u32, 0x8086_a201);
        write(&mut hpet, 0x100, 6 | (16 << 9));
        write(&mut hpet, 0x108, 100);
        write(&mut hpet, 0x10, 1);
        hpet.advance(990);
        assert!(!lines[16].high.load(Ordering::SeqCst));
        hpet.advance(10);
        assert!(lines[16].high.load(Ordering::SeqCst));
        assert_eq!(read(&mut hpet, 0x20), 1);
        write(&mut hpet, 0x20, 1);
        assert!(!lines[16].high.load(Ordering::SeqCst));
        write(&mut hpet, 0x120, 4);
        write(&mut hpet, 0x128, 200);
        write(&mut hpet, 0x10, 3);
        assert!(legacy.load(Ordering::SeqCst));
        hpet.advance(1000);
        assert_eq!(lines[8].rises.load(Ordering::SeqCst), 1);
        assert_eq!(read(&mut hpet, 0xf0), 200);
        write(&mut hpet, 0x100, 4);
        write(&mut hpet, 0x108, 300);
        hpet.advance(1000);
        assert_eq!(lines[2].rises.load(Ordering::SeqCst), 1);
    }
    #[test]
    fn p4_hpet_periodic_32bit_and_snapshot() {
        let lines: [Arc<Line>; 32] = std::array::from_fn(|_| Arc::new(Line::default()));
        let mut hpet = Hpet::new(
            std::array::from_fn(|i| lines[i].clone() as Arc<dyn IrqLine>),
            Arc::new(|_| {}),
        );
        write(&mut hpet, 0x100, 0x14c | (2 << 9));
        write(&mut hpet, 0x108, 20);
        write(&mut hpet, 0x108, 10);
        write(&mut hpet, 0x10, 1);
        hpet.advance(200);
        assert_eq!(lines[2].rises.load(Ordering::SeqCst), 1);
        hpet.advance(100);
        assert_eq!(lines[2].rises.load(Ordering::SeqCst), 2);
        let snapshot = hpet.save_state().unwrap();
        hpet.advance(100);
        hpet.restore_state(&snapshot).unwrap();
        assert_eq!(read(&mut hpet, 0xf0), 30);
        assert_eq!(read(&mut hpet, 0x108), 40);
        write(&mut hpet, 0x10, 0);
        hpet.advance(100);
        assert_eq!(read(&mut hpet, 0xf0), 30);
        write(&mut hpet, 0x100, 0x104 | (2 << 9));
        write(&mut hpet, 0xf0, u64::from(u32::MAX) - 2);
        write(&mut hpet, 0x108, 1);
        write(&mut hpet, 0x10, 1);
        hpet.advance(40);
        assert_eq!(lines[2].rises.load(Ordering::SeqCst), 4);
        assert!(hpet.restore_state(b"bad").is_err());
    }

    #[test]
    fn p4_hpet_split_dword_period_writes() {
        let lines: [Arc<Line>; 32] = std::array::from_fn(|_| Arc::new(Line::default()));
        let mut hpet = Hpet::new(
            std::array::from_fn(|i| lines[i].clone() as Arc<dyn IrqLine>),
            Arc::new(|_| {}),
        );
        write(&mut hpet, 0x100, 0x14c | (2 << 9));
        hpet.write(0x108, &20u32.to_le_bytes());
        hpet.write(0x10c, &0u32.to_le_bytes());
        hpet.write(0x108, &10u32.to_le_bytes());
        hpet.write(0x10c, &0u32.to_le_bytes());
        write(&mut hpet, 0x10, 1);
        hpet.advance(200);
        hpet.advance(100);
        assert_eq!(lines[2].rises.load(Ordering::SeqCst), 2);
        assert_eq!(read(&mut hpet, 0x108), 40);
    }
}
