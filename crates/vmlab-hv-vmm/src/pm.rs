//! The ACPI power-management register block and the power button.
//!
//! The device occupies the fixed I/O range `0x600..0x608` described by the
//! FADT and the DSDT (`PM1a_EVT_BLK` at 0x600, 4 bytes; `PM1a_CNT_BLK` at
//! 0x604, 2 bytes). The SCI is delivered on ISA IRQ 9 (GSI 9), which the
//! MADT describes. `powerdown` presses the power button: it latches
//! `PWRBTN_STS`, raises the SCI if the guest enabled the event, and the
//! guest OS then writes `SLP_TYP`/`SLP_EN` to enter S5 — which the VMM
//! observes here as the shutdown the guest asked for.

use std::sync::Arc;

use vmlab_hv_devices::{BusDevice, IrqLine};

/// PM1a_EVT_BLK base port.
pub const PM1A_EVT_BASE: u16 = 0x600;
/// PM1a_CNT_BLK base port.
pub const PM1A_CNT_BASE: u16 = 0x604;
/// Length of the whole block as reported in the FADT.
pub const PM_BLOCK_LEN: u16 = 6;
/// GSI of the SCI.
pub const SCI_GSI: u32 = 9;

/// SLP_TYP values published in the DSDT's `\_Sx` packages.
pub const SLP_TYP_S3: u16 = 3;
pub const SLP_TYP_S4: u16 = 4;
pub const SLP_TYP_S5: u16 = 5;

const STS_PWRBTN: u16 = 1 << 8;
const STS_WAK: u16 = 1 << 15;
const EN_PWRBTN: u16 = 1 << 8;
const CNT_SLP_TYP_MASK: u16 = 0x7 << 10;
const CNT_SLP_EN: u16 = 1 << 13;

/// Which sleep state the guest requested.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SleepState {
    S3,
    S4,
    S5,
}

/// Notified when the guest writes SLP_EN with a known SLP_TYP.
pub type OnSleep = Arc<dyn Fn(SleepState) + Send + Sync>;

/// The ACPI PM1a register block.
pub struct PmDevice {
    sci: Arc<dyn IrqLine>,
    on_sleep: OnSleep,
    sts: u16,
    en: u16,
    cnt: u16,
}

impl PmDevice {
    pub fn new(sci: Arc<dyn IrqLine>, on_sleep: OnSleep) -> Self {
        PmDevice {
            sci,
            on_sleep,
            sts: STS_WAK,
            en: 0,
            cnt: 0,
        }
    }

    /// Press the power button (the `powerdown` command).
    pub fn press_power_button(&mut self) {
        self.sts |= STS_PWRBTN;
        self.update_sci();
    }

    /// Level-assert the SCI while any enabled event is pending.
    fn update_sci(&self) {
        self.sci.set_level(self.sts & self.en != 0);
    }
}

impl BusDevice for PmDevice {
    fn read(&mut self, offset: u64, data: &mut [u8]) {
        let value = match offset {
            0..=1 => self.sts,
            2..=3 => self.en,
            4..=5 => self.cnt,
            _ => 0xFFFF,
        };
        fill_u16(within_register(offset), data, value);
    }

    fn write(&mut self, offset: u64, data: &[u8]) {
        let value = read_u16(within_register(offset), data);
        match offset {
            0..=1 => {
                // PM1a_STS is write-1-to-clear.
                self.sts &= !value;
                self.update_sci();
            }
            2..=3 => {
                self.en = value & EN_PWRBTN;
                self.update_sci();
            }
            4..=5 => {
                self.cnt = value & !CNT_SLP_EN;
                if value & CNT_SLP_EN != 0 {
                    let slp_typ = (value & CNT_SLP_TYP_MASK) >> 10;
                    // S4 hibernates the guest; the machine is done, as with
                    // S5 (this is what QEMU does too).
                    let state = match slp_typ {
                        SLP_TYP_S3 => SleepState::S3,
                        SLP_TYP_S4 | SLP_TYP_S5 => SleepState::S5,
                        _ => return,
                    };
                    (self.on_sleep)(state);
                }
            }
            _ => {}
        }
    }
}

/// Where a port access falls inside its 2-byte register: even ports address
/// the low byte, odd ports the high byte. Register blocks are 2-byte aligned
/// so this is the port offset modulo 2.
fn within_register(offset: u64) -> usize {
    (offset % 2) as usize
}

/// Read a u16 register from possibly-partial port data at `offset` within the
/// register (0 or 1).
fn read_u16(offset: usize, data: &[u8]) -> u16 {
    let mut bytes = [0u8; 2];
    let n = data.len().min(2 - offset);
    bytes[offset..offset + n].copy_from_slice(&data[..n]);
    u16::from_le_bytes(bytes)
}

fn fill_u16(offset: usize, data: &mut [u8], value: u16) {
    let bytes = value.to_le_bytes();
    let n = data.len().min(2 - offset);
    data[..n].copy_from_slice(&bytes[offset..offset + n]);
    data[n..].fill(0xFF);
}

/// Where the PM device parks a guest sleep request for the VMM to act on
/// after the port write returns.
#[derive(Default)]
pub struct SleepSignal {
    pending: parking_lot::Mutex<Option<SleepState>>,
}

impl SleepSignal {
    /// A closure suitable for [`PmDevice::new`]'s `on_sleep`.
    pub fn sink(self: &std::sync::Arc<Self>) -> OnSleep {
        let signal = self.clone();
        Arc::new(move |state| *signal.pending.lock() = Some(state))
    }

    /// Take the pending request, if any.
    pub fn take(&self) -> Option<SleepState> {
        self.pending.lock().take()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
    use parking_lot::Mutex;

    struct FakeIrq {
        level: AtomicBool,
    }

    impl IrqLine for FakeIrq {
        fn set_level(&self, high: bool) {
            self.level.store(high, Ordering::SeqCst);
        }
    }

    struct Sink {
        sleeps: Mutex<Vec<SleepState>>,
    }

    #[test]
    fn power_button_sets_status_and_sci_when_enabled() {
        let irq = Arc::new(FakeIrq {
            level: AtomicBool::new(false),
        });
        let sink = Arc::new(Sink {
            sleeps: Mutex::new(Vec::new()),
        });
        let sink2 = sink.clone();
        let mut pm = PmDevice::new(
            irq.clone(),
            Arc::new(move |s| sink2.sleeps.lock().push(s)),
        );

        // Button pressed with the event disabled: status latched, no SCI.
        pm.press_power_button();
        assert!(!irq.level.load(Ordering::SeqCst));
        let mut sts = [0u8; 2];
        pm.read(0, &mut sts);
        assert_eq!(u16::from_le_bytes(sts), STS_PWRBTN | STS_WAK);

        // Guest enables the event: SCI asserts.
        pm.write(2, &[0x00, 0x01]);
        assert!(irq.level.load(Ordering::SeqCst));

        // Guest acks: status clears and the SCI drops.
        pm.write(0, &[0x00, 0x01]);
        assert!(!irq.level.load(Ordering::SeqCst));
        pm.read(0, &mut sts);
        assert_eq!(u16::from_le_bytes(sts), STS_WAK);
    }

    #[test]
    fn s5_write_fires_sleep() {
        let irq = Arc::new(FakeIrq {
            level: AtomicBool::new(false),
        });
        let seen = Arc::new(AtomicU8::new(0));
        let seen2 = seen.clone();
        let mut pm = PmDevice::new(irq, Arc::new(move |s| {
            seen2.store(match s { SleepState::S3 => 3, SleepState::S4 | SleepState::S5 => 5 }, Ordering::SeqCst)
        }));
        // SLP_TYP=5, SLP_EN.
        pm.write(4, &0x3400u16.to_le_bytes());
        assert_eq!(seen.load(Ordering::SeqCst), 5);
        // SLP_TYP=3, SLP_EN -> suspend.
        pm.write(4, &0x2C00u16.to_le_bytes());
        assert_eq!(seen.load(Ordering::SeqCst), 3);
        // SLP_EN is not stored in the register image.
        let mut cnt = [0u8; 2];
        pm.read(4, &mut cnt);
        assert_eq!(u16::from_le_bytes(cnt), 0x0C00); // last SLP_TYP only
        // SLP_EN without a known SLP_TYP is ignored and zeroes the type.
        pm.write(4, &0x2000u16.to_le_bytes());
        assert_eq!(seen.load(Ordering::SeqCst), 3);
        pm.read(4, &mut cnt);
        assert_eq!(u16::from_le_bytes(cnt), 0);
    }
}
