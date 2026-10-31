//! MC146818-compatible RTC, including the PC extended CMOS bank.

use crate::{BusDevice, DeviceError, IrqLine, Snapshot};
use std::sync::Arc;

/// Register at I/O 0x70..=0x77. Advance with elapsed running guest time.
pub struct Rtc {
    cmos: [u8; 256],
    index: [u8; 2],
    seconds: i64,
    subsecond: u64,
    periodic_phase: u64,
    irq: Arc<dyn IrqLine>,
}

impl crate::Resettable for Rtc {
    fn reset(&mut self) -> Result<(), DeviceError> {
        self.index = [0; 2];
        self.cmos[0x0b] &= !0xf8;
        self.cmos[0x0c] = 0;
        self.periodic_phase = 0;
        self.update_irq();
        Ok(())
    }
}

impl Rtc {
    /// `rtc_base` is UTC Unix time in seconds; RAM includes memory above 4 GiB.
    pub fn new(ram_bytes: u64, rtc_base: i64, irq: Arc<dyn IrqLine>) -> Self {
        let mut rtc = Self {
            cmos: [0; 256],
            index: [0; 2],
            seconds: rtc_base,
            subsecond: 0,
            periodic_phase: 0,
            irq,
        };
        rtc.cmos[0x0a] = 0x26;
        rtc.cmos[0x0b] = 0x02;
        rtc.cmos[0x0d] = 0x80;
        rtc.cmos[0x14] = 0x06;
        rtc.cmos[0x15..0x17].copy_from_slice(&640u16.to_le_bytes());
        let low = ram_bytes.min(0xb000_0000);
        let ext = (low.saturating_sub(1 << 20) / 1024).min(65535) as u16;
        rtc.cmos[0x17..0x19].copy_from_slice(&ext.to_le_bytes());
        rtc.cmos[0x30..0x32].copy_from_slice(&ext.to_le_bytes());
        let above16 = (low.saturating_sub(16 << 20) >> 16) as u16;
        rtc.cmos[0x34..0x36].copy_from_slice(&above16.to_le_bytes());
        let high = (ram_bytes.saturating_sub(low) >> 16).min(0xff_ffff) as u32;
        rtc.cmos[0x5b..0x5e].copy_from_slice(&high.to_le_bytes()[..3]);
        rtc.update_calendar();
        rtc.irq.set_level(false);
        rtc
    }

    /// Override CMOS memory sizing when the machine uses a different PCI hole.
    pub fn set_memory_layout(&mut self, below_4g: u64, above_4g: u64) {
        let ext = (below_4g.saturating_sub(1 << 20) / 1024).min(65535) as u16;
        self.cmos[0x17..0x19].copy_from_slice(&ext.to_le_bytes());
        self.cmos[0x30..0x32].copy_from_slice(&ext.to_le_bytes());
        let above16 = (below_4g.saturating_sub(16 << 20) >> 16).min(65535) as u16;
        self.cmos[0x34..0x36].copy_from_slice(&above16.to_le_bytes());
        let high = (above_4g >> 16).min(0xff_ffff) as u32;
        self.cmos[0x5b..0x5e].copy_from_slice(&high.to_le_bytes()[..3]);
    }

    /// Progress calendar and interrupt sources using the VMM's running clock.
    pub fn advance(&mut self, ns: u64) {
        if self.cmos[0x0a] & 0x70 != 0x20 {
            return;
        }
        let rate = self.cmos[0x0a] & 15;
        if rate != 0 {
            let rate = if rate < 3 { rate + 7 } else { rate };
            let frequency = 32768u64 >> (rate - 1);
            let phase = u128::from(self.periodic_phase) + u128::from(ns) * u128::from(frequency);
            if phase >= 1_000_000_000 {
                self.cmos[0x0c] |= 0x40;
            }
            self.periodic_phase = (phase % 1_000_000_000) as u64;
        }
        if self.cmos[0x0b] & 0x80 == 0 {
            let elapsed = u128::from(self.subsecond) + u128::from(ns);
            let ticks = (elapsed / 1_000_000_000) as u64;
            self.subsecond = (elapsed % 1_000_000_000) as u64;
            // All alarm patterns repeat daily. Check at most one full day.
            for _ in 0..ticks.min(86400) {
                self.seconds = self.seconds.saturating_add(1);
                self.update_calendar();
                if [0usize, 2, 4]
                    .into_iter()
                    .all(|r| self.cmos[r + 1] & 0xc0 == 0xc0 || self.cmos[r + 1] == self.cmos[r])
                {
                    self.cmos[0x0c] |= 0x20;
                }
            }
            if ticks > 86400 {
                self.seconds = self.seconds.saturating_add((ticks - 86400) as i64);
                self.update_calendar();
            }
            if ticks != 0 {
                self.cmos[0x0c] |= 0x10;
            }
        }
        self.update_irq();
    }

    fn update_irq(&mut self) {
        let high = self.cmos[0x0b] & self.cmos[0x0c] & 0x70 != 0;
        self.cmos[0x0c] = (self.cmos[0x0c] & 0x7f) | if high { 0x80 } else { 0 };
        self.irq.set_level(high);
    }

    fn encode(&self, value: u8) -> u8 {
        if self.cmos[0x0b] & 4 != 0 {
            value
        } else {
            ((value / 10) << 4) | (value % 10)
        }
    }

    fn decode(&self, value: u8) -> u8 {
        if self.cmos[0x0b] & 4 != 0 {
            value
        } else {
            (value >> 4) * 10 + (value & 15)
        }
    }

    fn update_calendar(&mut self) {
        let days = self.seconds.div_euclid(86400);
        let day_time = self.seconds.rem_euclid(86400);
        let (year, month, day) = civil_from_days(days);
        self.cmos[0] = self.encode((day_time % 60) as u8);
        self.cmos[2] = self.encode((day_time / 60 % 60) as u8);
        let hour = (day_time / 3600) as u8;
        self.cmos[4] = if self.cmos[0x0b] & 2 != 0 {
            self.encode(hour)
        } else {
            self.encode(if hour.is_multiple_of(12) {
                12
            } else {
                hour % 12
            }) | if hour >= 12 { 0x80 } else { 0 }
        };
        self.cmos[6] = self.encode((days + 4).rem_euclid(7) as u8 + 1);
        self.cmos[7] = self.encode(day);
        self.cmos[8] = self.encode(month);
        self.cmos[9] = self.encode(year.rem_euclid(100) as u8);
        self.cmos[0x32] = self.encode((year / 100) as u8);
    }

    fn commit_calendar(&mut self) {
        let year =
            i64::from(self.decode(self.cmos[0x32])) * 100 + i64::from(self.decode(self.cmos[9]));
        let month = self.decode(self.cmos[8]).clamp(1, 12);
        let day = self.decode(self.cmos[7]).clamp(1, 31);
        let mut hour = self.decode(self.cmos[4] & 0x7f).min(23);
        if self.cmos[0x0b] & 2 == 0 {
            hour = hour % 12 + if self.cmos[4] & 0x80 != 0 { 12 } else { 0 };
        }
        self.seconds = days_from_civil(year, month, day) * 86400
            + i64::from(hour) * 3600
            + i64::from(self.decode(self.cmos[2]).min(59)) * 60
            + i64::from(self.decode(self.cmos[0]).min(59));
        self.subsecond = 0;
    }
}

impl BusDevice for Rtc {
    fn read(&mut self, offset: u64, data: &mut [u8]) {
        for (i, byte) in data.iter_mut().enumerate() {
            let port = offset.saturating_add(i as u64) & 7;
            let bank = ((port >> 1) & 1) as usize;
            if port & 1 == 0 {
                *byte = self.index[bank];
                continue;
            }
            let reg = usize::from(self.index[bank] & 0x7f) + bank * 128;
            *byte = self.cmos[reg];
            if reg == 0x0a && self.cmos[0x0b] & 0x80 == 0 && self.subsecond >= 999_756_000 {
                *byte |= 0x80;
            }
            if reg == 0x0c {
                self.cmos[reg] = 0;
                self.update_irq();
            }
        }
    }
    fn write(&mut self, offset: u64, data: &[u8]) {
        for (i, &value) in data.iter().enumerate() {
            let port = offset.saturating_add(i as u64) & 7;
            let bank = ((port >> 1) & 1) as usize;
            if port & 1 == 0 {
                self.index[bank] = value;
                continue;
            }
            let reg = usize::from(self.index[bank] & 0x7f) + bank * 128;
            match reg {
                0x0c | 0x0d => (),
                0x0a => {
                    self.cmos[reg] = value & 0x7f;
                    self.periodic_phase = 0;
                }
                0x0b => {
                    let previous = self.cmos[reg];
                    self.cmos[reg] = value;
                    if previous & 0x80 != 0 && value & 0x80 == 0 {
                        self.commit_calendar();
                    } else if value & 0x80 == 0 {
                        self.update_calendar();
                    }
                }
                _ => {
                    self.cmos[reg] = value;
                    if matches!(reg, 0 | 2 | 4 | 7 | 8 | 9 | 0x32) && self.cmos[0x0b] & 0x80 == 0 {
                        self.commit_calendar();
                    }
                }
            }
            self.update_irq();
        }
    }
}

impl Snapshot for Rtc {
    fn state_id(&self) -> String {
        "chipset.rtc".into()
    }
    fn save_state(&self) -> Result<Vec<u8>, DeviceError> {
        let mut bytes = b"RTC1".to_vec();
        bytes.extend_from_slice(&self.cmos);
        bytes.extend_from_slice(&self.index);
        bytes.extend_from_slice(&self.seconds.to_le_bytes());
        bytes.extend_from_slice(&self.subsecond.to_le_bytes());
        bytes.extend_from_slice(&self.periodic_phase.to_le_bytes());
        Ok(bytes)
    }
    fn restore_state(&mut self, state: &[u8]) -> Result<(), DeviceError> {
        if state.len() != 286 || &state[..4] != b"RTC1" {
            return Err(DeviceError::State("invalid RTC state".into()));
        }
        let seconds = i64::from_le_bytes(state[262..270].try_into().unwrap());
        let subsecond = u64::from_le_bytes(state[270..278].try_into().unwrap());
        let phase = u64::from_le_bytes(state[278..286].try_into().unwrap());
        if subsecond >= 1_000_000_000 || phase >= 1_000_000_000 {
            return Err(DeviceError::State("invalid RTC phase".into()));
        }
        self.cmos.copy_from_slice(&state[4..260]);
        self.index.copy_from_slice(&state[260..262]);
        self.seconds = seconds;
        self.subsecond = subsecond;
        self.periodic_phase = phase;
        self.update_irq();
        Ok(())
    }
}

fn civil_from_days(days: i64) -> (i64, u8, u8) {
    let z = days + 719468;
    let era = z.div_euclid(146097);
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let year = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = mp + if mp < 10 { 3 } else { -9 };
    (year + i64::from(month <= 2), month as u8, day as u8)
}

fn days_from_civil(year: i64, month: u8, day: u8) -> i64 {
    let year = year - i64::from(month <= 2);
    let era = year.div_euclid(400);
    let yoe = year - era * 400;
    let mp = i64::from(month) + if month > 2 { -3 } else { 9 };
    let doy = (153 * mp + 2) / 5 + i64::from(day) - 1;
    era * 146097 + yoe * 365 + yoe / 4 - yoe / 100 + doy - 719468
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};
    #[derive(Default)]
    struct Line(AtomicBool);
    impl IrqLine for Line {
        fn set_level(&self, high: bool) {
            self.0.store(high, Ordering::SeqCst);
        }
    }
    fn get(rtc: &mut Rtc, reg: u8) -> u8 {
        rtc.write(0, &[reg]);
        let mut b = [0];
        rtc.read(1, &mut b);
        b[0]
    }
    fn put(rtc: &mut Rtc, reg: u8, value: u8) {
        rtc.write(0, &[reg]);
        rtc.write(1, &[value]);
    }
    #[test]
    fn system_reset_preserves_calendar_and_cmos_but_clears_interrupts() {
        use crate::Resettable;
        let irq = Arc::new(Line::default());
        let mut rtc = Rtc::new(2 << 30, 1_793_355_112, irq.clone());
        put(&mut rtc, 0x38, 0x42);
        put(&mut rtc, 0x0b, 0x12);
        rtc.advance(5_000_000_000);
        let seconds = rtc.seconds;
        assert!(irq.0.load(Ordering::SeqCst));
        rtc.reset().unwrap();
        assert_eq!(rtc.seconds, seconds);
        assert_eq!(get(&mut rtc, 0x38), 0x42);
        assert_eq!(get(&mut rtc, 0x0b), 2);
        assert_eq!(get(&mut rtc, 0x0c), 0);
        assert!(!irq.0.load(Ordering::SeqCst));
    }
    #[test]
    fn p4_calendar_update_alarm_and_periodic_irq() {
        let irq = Arc::new(Line::default());
        let mut rtc = Rtc::new(2 << 30, 1_793_355_112, irq.clone());
        assert_eq!(get(&mut rtc, 9), 0x26);
        assert_eq!(get(&mut rtc, 8), 0x10);
        assert_eq!(get(&mut rtc, 7), 0x30);
        put(&mut rtc, 0x0b, 0x12);
        rtc.advance(1_000_000_000);
        assert!(irq.0.load(Ordering::SeqCst));
        assert_eq!(get(&mut rtc, 0x0c) & 0x90, 0x90);
        assert!(!irq.0.load(Ordering::SeqCst));
        put(&mut rtc, 1, 0xc0);
        put(&mut rtc, 3, 0xc0);
        put(&mut rtc, 5, 0xc0);
        put(&mut rtc, 0x0b, 0x22);
        rtc.advance(1_000_000_000);
        assert_eq!(get(&mut rtc, 0x0c) & 0xa0, 0xa0);
        put(&mut rtc, 0x0b, 0x42);
        rtc.advance(976_563);
        assert_eq!(get(&mut rtc, 0x0c) & 0xc0, 0xc0);
    }
    #[test]
    fn p4_set_binary_time_memory_and_snapshot() {
        let mut rtc = Rtc::new(8 << 30, 0, Arc::new(Line::default()));
        rtc.set_memory_layout(3 << 30, 5 << 30);
        assert_eq!(get(&mut rtc, 0x35), 0xbf);
        assert_eq!(get(&mut rtc, 0x5d), 1);
        put(&mut rtc, 0x0b, 0x86);
        for (reg, value) in [
            (0x32, 20),
            (9, 24),
            (8, 2),
            (7, 28),
            (4, 23),
            (2, 59),
            (0, 59),
        ] {
            put(&mut rtc, reg, value);
        }
        put(&mut rtc, 0x0b, 6);
        rtc.advance(1_000_000_000);
        assert_eq!(get(&mut rtc, 7), 29);
        put(&mut rtc, 0x40, 0xab);
        let snapshot = rtc.save_state().unwrap();
        rtc.advance(86_400_000_000_000);
        rtc.restore_state(&snapshot).unwrap();
        assert_eq!(get(&mut rtc, 7), 29);
        assert_eq!(get(&mut rtc, 0x40), 0xab);
        assert!(rtc.restore_state(b"bad").is_err());
    }

    #[test]
    fn p4_uip_12hour_and_extended_cmos() {
        let mut rtc = Rtc::new(1 << 30, 12 * 3600, Arc::new(Line::default()));
        put(&mut rtc, 0x0b, 0);
        assert_eq!(get(&mut rtc, 4), 0x92);
        rtc.advance(999_800_000);
        assert_ne!(get(&mut rtc, 0x0a) & 0x80, 0);
        put(&mut rtc, 0x0b, 0x80);
        assert_eq!(get(&mut rtc, 0x0a) & 0x80, 0);
        rtc.advance(2_000_000_000);
        assert_eq!(get(&mut rtc, 0), 0);
        rtc.write(2, &[0x20]);
        rtc.write(3, &[0xac]);
        rtc.write(6, &[0x20]);
        let mut byte = [0];
        rtc.read(7, &mut byte);
        assert_eq!(byte, [0xac]);
        rtc.write(4, &[0x40]);
        rtc.write(5, &[0x55]);
        assert_eq!(get(&mut rtc, 0x40), 0x55);
        rtc.read(u64::MAX, &mut [0; 2]);
        rtc.write(u64::MAX, &[0, 0]);
    }
}
