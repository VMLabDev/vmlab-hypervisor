//! ICH9 LPC function (00:1f.0), ACPI PM registers and legacy reset ports.
//!
//! Register definitions follow Intel's ICH9 Family Datasheet, sections 13/14,
//! and ACPI's fixed hardware registers. Sleep encodings follow the captured
//! q35 DSDT, rather than the encodings of the phase-1 microvm.

use std::sync::Arc;

use crate::{BarInfo, BusDevice, DeviceError, IrqLine, PciDevice, Snapshot};

const PM_HZ: u128 = 3_579_545;
const STATUS_MASK: u16 = 0x8d31;
const ENABLE_MASK: u16 = 0x0521;

/// Requests delivered to the machine outside the chipset model.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PowerAction {
    Suspend,
    Hibernate,
    PowerOff,
    Reset,
}

/// PCI config and the PMBASE-relative I/O window share one device instance.
/// Route ports b2/b3/cf9 through [`Self::read_port`]/[`Self::write_port`].
/// The PM window is 128 bytes; its base/enable may change after config writes.
pub struct Lpc {
    config: [u8; 256],
    pm_status: u16,
    pm_enable: u16,
    pm_control: u16,
    gpe_status: [u8; 8],
    gpe_enable: [u8; 8],
    timer_ticks: u64,
    timer_fraction: u64,
    apm: [u8; 2],
    reset_control: u8,
    sci: Arc<dyn IrqLine>,
    action: Arc<dyn Fn(PowerAction) + Send + Sync>,
}

impl Lpc {
    pub fn new(sci: Arc<dyn IrqLine>, action: Arc<dyn Fn(PowerAction) + Send + Sync>) -> Self {
        let mut config = [0; 256];
        config[..4].copy_from_slice(&[0x86, 0x80, 0x18, 0x29]);
        config[8..12].copy_from_slice(&[2, 0, 1, 6]);
        config[0x0e] = 0x80;
        config[0x2c..0x30].copy_from_slice(&[0xf4, 0x1a, 0, 0x11]);
        config[0x40] = 1;
        config[0x60..0x64].fill(0x80);
        config[0x68..0x6c].fill(0x80);
        config[0x82] = 5;
        let device = Self {
            config,
            pm_status: 0,
            pm_enable: 0,
            pm_control: 0,
            gpe_status: [0; 8],
            gpe_enable: [0; 8],
            timer_ticks: 0,
            timer_fraction: 0,
            apm: [0; 2],
            reset_control: 0,
            sci,
            action,
        };
        device.update_sci();
        device
    }

    /// Legacy PIC destination of PIRQ A through H; None means disabled.
    /// The VMM must recompute routing after writes to 60-63h or 68-6bh.
    /// APIC-mode PCI routing to GSI16-23 is described separately by `_PRT`.
    pub fn pirq_route(&self, pirq: usize) -> Option<u8> {
        let offset = match pirq {
            0..=3 => 0x60 + pirq,
            4..=7 => 0x68 + pirq - 4,
            _ => return None,
        };
        let route = self.config[offset];
        (route & 0x80 == 0).then_some(route & 15)
    }

    pub fn pm_base(&self) -> u16 {
        u16::from_le_bytes([self.config[0x40], self.config[0x41]]) & 0xff80
    }

    pub fn pm_enabled(&self) -> bool {
        self.config[0x44] & 0x80 != 0
    }

    /// Advance only while the machine runs. The counter is 24 bits, as FACP
    /// declares. ACPI TMR_STS is set on transitions of its most significant bit.
    pub fn advance(&mut self, ns: u64) {
        let scaled = ns as u128 * PM_HZ + self.timer_fraction as u128;
        let ticks = (scaled / 1_000_000_000) as u64;
        self.timer_fraction = (scaled % 1_000_000_000) as u64;
        if ticks >= (1 << 23) || (self.timer_ticks & 0x7fffff) + ticks >= (1 << 23) {
            self.pm_status |= 1;
        }
        self.timer_ticks = self.timer_ticks.wrapping_add(ticks);
        self.update_sci();
    }

    pub fn power_button(&mut self) {
        self.pm_status |= 1 << 8;
        self.update_sci();
    }

    pub fn rtc_alarm(&mut self) {
        self.pm_status |= 1 << 10;
        self.update_sci();
    }

    pub fn raise_gpe(&mut self, event: u8) {
        if event < 64 {
            self.gpe_status[event as usize / 8] |= 1 << (event % 8);
            self.update_sci();
        }
    }

    pub fn wakeup(&mut self) {
        self.pm_status |= 1 << 15;
        self.update_sci();
    }

    fn update_sci(&self) {
        let pending = self.pm_status & self.pm_enable != 0
            || self
                .gpe_status
                .iter()
                .zip(self.gpe_enable)
                .any(|(s, e)| s & e != 0);
        self.sci.set_level(self.pm_control & 1 != 0 && pending);
    }

    /// Absolute port accesses for the three sparse LPC I/O ranges. Returns
    /// false for an unclaimed port, so the machine can route another device.
    pub fn read_port(&mut self, port: u16, data: &mut [u8]) -> bool {
        if self.pm_enabled() && port.wrapping_sub(self.pm_base()) < 128 {
            self.read(u64::from(port - self.pm_base()), data);
            true
        } else if matches!(port, 0xb2 | 0xb3 | 0xcf9) && data.len() == 1 {
            data[0] = if port == 0xcf9 {
                self.reset_control
            } else {
                self.apm[(port - 0xb2) as usize]
            };
            true
        } else {
            false
        }
    }

    pub fn write_port(&mut self, port: u16, data: &[u8]) -> bool {
        if self.pm_enabled() && port.wrapping_sub(self.pm_base()) < 128 {
            self.write(u64::from(port - self.pm_base()), data);
            true
        } else if matches!(port, 0xb2 | 0xb3 | 0xcf9) && data.len() == 1 {
            if port == 0xcf9 {
                let previous = self.reset_control;
                self.reset_control = data[0] & 0x0e;
                if previous & 4 == 0 && data[0] & 4 != 0 {
                    (self.action)(PowerAction::Reset);
                }
            } else {
                self.apm[(port - 0xb2) as usize] = data[0];
                if port == 0xb2 {
                    match data[0] {
                        2 => self.pm_control |= 1,
                        3 => self.pm_control &= !1,
                        _ => {}
                    }
                    self.update_sci();
                }
            }
            true
        } else {
            false
        }
    }
}

impl BusDevice for Lpc {
    fn read(&mut self, offset: u64, data: &mut [u8]) {
        for (i, byte) in data.iter_mut().enumerate() {
            *byte = match offset.checked_add(i as u64) {
                Some(o @ 0..=1) => self.pm_status.to_le_bytes()[o as usize],
                Some(o @ 2..=3) => self.pm_enable.to_le_bytes()[o as usize - 2],
                Some(o @ 4..=5) => self.pm_control.to_le_bytes()[o as usize - 4],
                Some(o @ 8..=11) => {
                    ((self.timer_ticks & 0xffffff) as u32).to_le_bytes()[o as usize - 8]
                }
                Some(o @ 0x20..=0x27) => self.gpe_status[o as usize - 0x20],
                Some(o @ 0x28..=0x2f) => self.gpe_enable[o as usize - 0x28],
                _ => 0,
            };
        }
    }

    fn write(&mut self, offset: u64, data: &[u8]) {
        let mut sleep = false;
        for (i, byte) in data.iter().copied().enumerate() {
            match offset.checked_add(i as u64) {
                Some(o @ 0..=1) => self.pm_status &= !(u16::from(byte) << (o * 8)),
                Some(o @ 2..=3) => {
                    let shift = (o - 2) * 8;
                    self.pm_enable = ((self.pm_enable & !(0xff << shift))
                        | (u16::from(byte) << shift))
                        & ENABLE_MASK;
                }
                Some(o @ 4..=5) => {
                    let shift = (o - 4) * 8;
                    self.pm_control = ((self.pm_control & !(0xff << shift))
                        | (u16::from(byte) << shift))
                        & 0x1c03;
                    sleep |= o == 5 && byte & 0x20 != 0;
                }
                Some(o @ 0x20..=0x27) => self.gpe_status[o as usize - 0x20] &= !byte,
                Some(o @ 0x28..=0x2f) => self.gpe_enable[o as usize - 0x28] = byte,
                _ => {}
            }
        }
        self.update_sci();
        if sleep {
            let action = match (self.pm_control >> 10) & 7 {
                0 => Some(PowerAction::PowerOff),
                1 => Some(PowerAction::Suspend),
                2 => Some(PowerAction::Hibernate),
                _ => None,
            };
            if let Some(action) = action {
                (self.action)(action);
            }
        }
    }
}

impl PciDevice for Lpc {
    fn config_read(&mut self, reg: usize) -> u32 {
        reg.checked_mul(4)
            .and_then(|o| self.config.get(o..o.checked_add(4)?))
            .map_or(u32::MAX, |b| u32::from_le_bytes(b.try_into().unwrap()))
    }

    fn config_write(&mut self, reg: usize, offset: u64, data: &[u8]) {
        if offset > 3 || data.len() > 4 - offset as usize {
            return;
        }
        let Some(start) = reg
            .checked_mul(4)
            .and_then(|n| n.checked_add(offset as usize))
        else {
            return;
        };
        for (i, byte) in data.iter().copied().enumerate() {
            let Some(index) = start.checked_add(i).filter(|n| *n < 256) else {
                break;
            };
            let mask = match index {
                4 => 0x0f,
                5 => 1,
                0x3c => 0xff,
                0x40 => 0x80,
                0x41 => 0xff,
                0x44 => 0x80,
                0x60..=0x63 | 0x68..=0x6b => 0x8f,
                0x4e..=0x4f | 0x80..=0x93 | 0xa0..=0xab => 0xff,
                0xf0 => 1,
                0xf1 => 0xc0,
                0xf2 | 0xf3 => 0xff,
                _ => 0,
            };
            self.config[index] = (self.config[index] & !mask) | (byte & mask);
        }
    }

    fn bars(&self) -> Vec<BarInfo> {
        Vec::new()
    }
    fn bar_read(&mut self, _bar: u8, _offset: u64, data: &mut [u8]) {
        data.fill(0xff);
    }
    fn bar_write(&mut self, _bar: u8, _offset: u64, _data: &[u8]) {}
}

impl Snapshot for Lpc {
    fn state_id(&self) -> String {
        "ich9-lpc".into()
    }

    fn save_state(&self) -> Result<Vec<u8>, DeviceError> {
        let mut state = b"LPC1".to_vec();
        state.extend(self.config);
        state.extend(self.pm_status.to_le_bytes());
        state.extend(self.pm_enable.to_le_bytes());
        state.extend(self.pm_control.to_le_bytes());
        state.extend(self.gpe_status);
        state.extend(self.gpe_enable);
        state.extend(self.timer_ticks.to_le_bytes());
        state.extend(self.timer_fraction.to_le_bytes());
        state.extend(self.apm);
        state.push(self.reset_control);
        Ok(state)
    }

    fn restore_state(&mut self, state: &[u8]) -> Result<(), DeviceError> {
        if state.len() != 301 || &state[..4] != b"LPC1" {
            return Err(DeviceError::State(
                "invalid LPC snapshot version/length".into(),
            ));
        }
        let status = u16::from_le_bytes(state[260..262].try_into().unwrap());
        let enable = u16::from_le_bytes(state[262..264].try_into().unwrap());
        let control = u16::from_le_bytes(state[264..266].try_into().unwrap());
        let fraction = u64::from_le_bytes(state[290..298].try_into().unwrap());
        if status & !STATUS_MASK != 0
            || enable & !ENABLE_MASK != 0
            || control & !0x1c03 != 0
            || fraction >= 1_000_000_000
        {
            return Err(DeviceError::State("invalid LPC register state".into()));
        }
        self.config.copy_from_slice(&state[4..260]);
        self.pm_status = status;
        self.pm_enable = enable;
        self.pm_control = control;
        self.gpe_status.copy_from_slice(&state[266..274]);
        self.gpe_enable.copy_from_slice(&state[274..282]);
        self.timer_ticks = u64::from_le_bytes(state[282..290].try_into().unwrap());
        self.timer_fraction = fraction;
        self.apm.copy_from_slice(&state[298..300]);
        self.reset_control = state[300];
        self.update_sci();
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use parking_lot::Mutex;
    use std::sync::atomic::{AtomicBool, Ordering};

    #[derive(Default)]
    struct Irq(AtomicBool);
    impl IrqLine for Irq {
        fn set_level(&self, high: bool) {
            self.0.store(high, Ordering::SeqCst);
        }
    }

    fn fixture() -> (Lpc, Arc<Irq>, Arc<Mutex<Vec<PowerAction>>>) {
        let irq = Arc::new(Irq::default());
        let actions = Arc::new(Mutex::new(Vec::new()));
        let sink = actions.clone();
        (
            Lpc::new(irq.clone(), Arc::new(move |a| sink.lock().push(a))),
            irq,
            actions,
        )
    }

    #[test]
    fn ovmf_can_assign_the_lpc_interrupt_line() {
        let (mut lpc, _, _) = fixture();
        lpc.config_write(0x3c / 4, 0, &[0xff]);
        assert_eq!(lpc.config_read(0x3c / 4) & 0xff, 0xff);
    }

    #[test]
    fn smi_lock_is_clear_at_reset() {
        let (mut lpc, _, _) = fixture();
        assert_eq!(lpc.config_read(0xa0 / 4) & 0x10, 0);
    }

    #[test]
    fn ovmf_pm_decode_does_not_require_pci_command_io_enable() {
        let (mut lpc, _, _) = fixture();
        lpc.config_write(0x40 / 4, 0, &0x601u32.to_le_bytes());
        lpc.config_write(0x44 / 4, 0, &[0x80]);
        assert_eq!(lpc.config_read(1) & 1, 0);
        lpc.advance(1_000_000_000);
        let mut data = [0; 4];
        assert!(lpc.read_port(0x608, &mut data));
        assert_eq!(u32::from_le_bytes(data), 3_579_545);
    }

    #[test]
    fn firmware_can_program_the_complete_rcba_address() {
        let (mut lpc, _, _) = fixture();
        lpc.config_write(0xf0 / 4, 0, &0xfed1c001u32.to_le_bytes());
        assert_eq!(lpc.config_read(0xf0 / 4), 0xfed1c001);
        lpc.config_write(0xf0 / 4, 0, &[0xff; 4]);
        assert_eq!(lpc.config_read(0xf0 / 4), 0xffffc001);
    }

    #[test]
    fn firmware_config_pirq_and_relocated_pm_decode() {
        let (mut lpc, _, _) = fixture();
        assert_eq!(lpc.config_read(0), 0x29188086);
        lpc.config_write(0, 0, &[0; 4]);
        assert_eq!(lpc.config_read(0), 0x29188086);
        assert_eq!(lpc.config_read(2), 0x06010002);
        assert_eq!(lpc.config_read(0x2c / 4), 0x11001af4);
        assert_eq!(lpc.config_read(usize::MAX), u32::MAX);
        assert_eq!(lpc.config_read(usize::MAX / 4), u32::MAX);
        lpc.config_write(usize::MAX, 0, &[0xff; 4]);
        lpc.config_write(usize::MAX / 4, 3, &[0xff]);
        assert_eq!(lpc.pirq_route(0), None);
        for i in 0..8 {
            let o = if i < 4 { 0x60 + i } else { 0x68 + i - 4 };
            lpc.config_write(o / 4, (o % 4) as u64, &[10 + i as u8 % 2]);
            assert_eq!(lpc.pirq_route(i), Some(10 + i as u8 % 2));
        }
        lpc.config_write(0x40 / 4, 0, &[1, 6]);
        lpc.config_write(0x44 / 4, 0, &[0x80]);
        assert!(lpc.write_port(0x602, &[1, 1]));
        lpc.config_write(1, 0, &[1]);
        assert_eq!(lpc.pm_base(), 0x600);
        assert!(lpc.write_port(0x602, &[1, 1]));
        lpc.config_write(0x40 / 4, 0, &[1, 8]);
        assert!(!lpc.read_port(0x608, &mut [0; 4]));
        assert!(lpc.read_port(0x808, &mut [0; 4]));
    }

    #[test]
    fn acpi_button_gpe_enable_disable_and_write_one_clear() {
        let (mut lpc, irq, _) = fixture();
        lpc.write(2, &0x100u16.to_le_bytes());
        lpc.power_button();
        assert!(!irq.0.load(Ordering::SeqCst));
        lpc.write_port(0xb2, &[2]);
        assert!(irq.0.load(Ordering::SeqCst));
        lpc.write(0, &0x100u16.to_le_bytes());
        assert!(!irq.0.load(Ordering::SeqCst));
        lpc.raise_gpe(42);
        lpc.write(0x2d, &[4]);
        assert!(irq.0.load(Ordering::SeqCst));
        lpc.write_port(0xb2, &[3]);
        assert!(!irq.0.load(Ordering::SeqCst));
        lpc.write_port(0xb2, &[2]);
        lpc.write(0x25, &[4]);
        assert!(!irq.0.load(Ordering::SeqCst));
    }

    #[test]
    fn timer_frequency_wrap_interrupt_and_snapshot_phase() {
        let (mut lpc, irq, _) = fixture();
        lpc.write(2, &[1]);
        lpc.write_port(0xb2, &[2]);
        lpc.advance(1_000_000_000);
        let mut timer = [0; 4];
        lpc.read(8, &mut timer);
        assert_eq!(u32::from_le_bytes(timer), 3_579_545);
        lpc.advance(2_000_000_000);
        assert!(irq.0.load(Ordering::SeqCst));
        lpc.write(0, &[1]);
        assert!(!irq.0.load(Ordering::SeqCst));
        lpc.advance(2_000_000_000);
        lpc.read(8, &mut timer);
        assert_eq!(u32::from_le_bytes(timer), (5 * 3_579_545) & 0xffffff);
        assert!(irq.0.load(Ordering::SeqCst));
        lpc.advance(1);
        let state = lpc.save_state().unwrap();
        lpc.advance(100);
        lpc.restore_state(&state).unwrap();
        assert_eq!(lpc.save_state().unwrap(), state);
        let mut bad = state.clone();
        bad[290..298].copy_from_slice(&1_000_000_000u64.to_le_bytes());
        assert!(lpc.restore_state(&bad).is_err());
        assert_eq!(lpc.save_state().unwrap(), state);
        for len in 0..state.len() {
            assert!(lpc.restore_state(&state[..len]).is_err());
        }
    }

    #[test]
    fn rtc_wake_apm_status_and_pending_sci_snapshot() {
        let (mut lpc, irq, actions) = fixture();
        lpc.write(2, &0x400u16.to_le_bytes());
        lpc.write_port(0xb2, &[2]);
        lpc.rtc_alarm();
        lpc.wakeup();
        assert!(irq.0.load(Ordering::SeqCst));
        let state = lpc.save_state().unwrap();
        lpc.write(0, &0xffffu16.to_le_bytes());
        assert!(!irq.0.load(Ordering::SeqCst));
        lpc.restore_state(&state).unwrap();
        assert!(irq.0.load(Ordering::SeqCst));
        assert!(actions.lock().is_empty());
        let mut status = [0; 2];
        lpc.read(0, &mut status);
        assert_eq!(u16::from_le_bytes(status), 0x8400);
        lpc.write_port(0xb3, &[0x87]);
        assert!(lpc.read_port(0xb3, &mut status[..1]));
        assert_eq!(status[0], 0x87);
    }

    #[test]
    fn dsdt_sleep_types_and_cf9_reset_edge() {
        let (mut lpc, _, actions) = fixture();
        for typ in [1u16, 2, 0, 7] {
            lpc.write(4, &(0x2000 | typ << 10).to_le_bytes());
        }
        lpc.write_port(0xcf9, &[2]);
        lpc.write_port(0xcf9, &[6]);
        lpc.write_port(0xcf9, &[6]);
        assert_eq!(
            *actions.lock(),
            [
                PowerAction::Suspend,
                PowerAction::Hibernate,
                PowerAction::PowerOff,
                PowerAction::Reset
            ]
        );
        let mut control = [0; 2];
        lpc.read(4, &mut control);
        assert_eq!(u16::from_le_bytes(control) & 0x2000, 0);
    }
}
