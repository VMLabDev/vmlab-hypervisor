//! ICH9 SMBus host controller, with no attached slaves.
//!
//! Register definitions follow Intel's ICH9 Family Datasheet, chapter 19.
//! PCI identity follows the q35 hardware captures. Firmware allocates BAR4
//! and enables the host at configuration byte 0x40 before starting transfers.

use std::sync::Arc;

use crate::{BarInfo, BarKind, BusDevice, DeviceError, IrqLine, PciDevice, Snapshot};

const DEV_ERR: u8 = 0x04;
const STATUS_W1C: u8 = 0x9e;
const SNAPSHOT_MAGIC: &[u8; 4] = b"SMB1";

/// PCI function 00:1f.3. Connect `irq` to its routed INTA interrupt line.
pub struct Smbus {
    id: String,
    config: [u8; 256],
    registers: [u8; 64],
    irq: Arc<dyn IrqLine>,
}

impl Smbus {
    pub fn new(id: impl Into<String>, irq: Arc<dyn IrqLine>) -> Self {
        let mut config = [0; 256];
        config[..4].copy_from_slice(&[0x86, 0x80, 0x30, 0x29]);
        config[8..12].copy_from_slice(&[2, 0, 5, 0x0c]);
        config[0x0e] = 0x80;
        config[0x20] = 1;
        config[0x2c..0x30].copy_from_slice(&[0xf4, 0x1a, 0, 0x11]);
        config[0x3d] = 1;
        irq.set_level(false);
        Self {
            id: id.into(),
            config,
            registers: [0; 64],
            irq,
        }
    }

    /// Firmware-selected base of the 64-byte BAR4 I/O window.
    pub fn io_base(&self) -> u16 {
        u16::from_le_bytes([self.config[0x20], self.config[0x21]]) & !63
    }

    /// Whether PCI I/O decode and SMBus host operation are both enabled.
    pub fn enabled(&self) -> bool {
        self.config[4] & 1 != 0 && self.config[0x40] & 1 != 0
    }

    fn update_irq(&self) {
        let pending = self.enabled()
            && self.config[0x40] & 2 == 0
            && self.config[5] & 4 == 0
            && self.registers[2] & 1 != 0
            && self.registers[0] & 0x1e != 0;
        self.irq.set_level(pending);
    }

    fn write_register(&mut self, offset: usize, value: u8) {
        match offset {
            0 => self.registers[0] &= !(value & STATUS_W1C),
            2 => {
                // START is write-only. No slave acknowledges any address.
                self.registers[2] = value & 0xbf;
                if value & 0x42 == 0x40 && self.enabled() {
                    self.registers[0] |= DEV_ERR;
                }
            }
            3..=7 | 0x0d | 0x10..=0x13 => self.registers[offset] = value,
            0x0c => self.registers[0x0c] &= !(value & 1),
            _ => {}
        }
        self.update_irq();
    }
}

impl BusDevice for Smbus {
    fn read(&mut self, offset: u64, data: &mut [u8]) {
        for (index, byte) in data.iter_mut().enumerate() {
            *byte = offset
                .checked_add(index as u64)
                .and_then(|offset| usize::try_from(offset).ok())
                .and_then(|offset| self.registers.get(offset).copied())
                .unwrap_or(0xff);
        }
    }

    fn write(&mut self, offset: u64, data: &[u8]) {
        for (index, &byte) in data.iter().enumerate() {
            if let Some(offset) = offset.checked_add(index as u64)
                && offset < self.registers.len() as u64
            {
                self.write_register(offset as usize, byte);
            }
        }
    }
}

impl PciDevice for Smbus {
    fn config_read(&mut self, reg: usize) -> u32 {
        reg.checked_mul(4)
            .and_then(|start| self.config.get(start..start.checked_add(4)?))
            .map(|bytes| u32::from_le_bytes(bytes.try_into().unwrap()))
            .unwrap_or(u32::MAX)
    }

    fn config_write(&mut self, reg: usize, offset: u64, data: &[u8]) {
        if offset > 3 || data.len() > 4 - offset as usize {
            return;
        }
        let Some(start) = reg
            .checked_mul(4)
            .and_then(|reg| reg.checked_add(offset as usize))
        else {
            return;
        };
        for (index, &byte) in data.iter().enumerate() {
            let Some(offset) = start.checked_add(index).filter(|&offset| offset < 256) else {
                break;
            };
            match offset {
                4 => self.config[4] = byte & 3,
                5 => self.config[5] = byte & 5,
                0x20 => self.config[offset] = (byte & 0xc0) | 1,
                0x21..=0x23 | 0x3c => self.config[offset] = byte,
                0x40 => self.config[offset] = byte & 0x1f,
                _ => {}
            }
        }
        self.update_irq();
    }

    fn bars(&self) -> Vec<BarInfo> {
        vec![BarInfo {
            index: 4,
            size: 64,
            kind: BarKind::Io,
        }]
    }

    fn bar_read(&mut self, bar: u8, offset: u64, data: &mut [u8]) {
        if bar == 4 && self.config[4] & 1 != 0 {
            self.read(offset, data);
        } else {
            data.fill(0xff);
        }
    }

    fn bar_write(&mut self, bar: u8, offset: u64, data: &[u8]) {
        if bar == 4 && self.config[4] & 1 != 0 {
            self.write(offset, data);
        }
    }
}

impl Snapshot for Smbus {
    fn state_id(&self) -> String {
        self.id.clone()
    }

    fn save_state(&self) -> Result<Vec<u8>, DeviceError> {
        let mut state = SNAPSHOT_MAGIC.to_vec();
        state.extend_from_slice(&self.config);
        state.extend_from_slice(&self.registers);
        Ok(state)
    }

    fn restore_state(&mut self, state: &[u8]) -> Result<(), DeviceError> {
        if state.len() != 324 || &state[..4] != SNAPSHOT_MAGIC {
            return Err(DeviceError::State("invalid SMBus snapshot".into()));
        }
        self.config.copy_from_slice(&state[4..260]);
        self.registers.copy_from_slice(&state[260..]);
        self.update_irq();
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicBool, Ordering};

    use super::*;

    #[derive(Default)]
    struct Line(AtomicBool);
    impl IrqLine for Line {
        fn set_level(&self, high: bool) {
            self.0.store(high, Ordering::SeqCst);
        }
    }

    fn controller() -> (Smbus, Arc<Line>) {
        let irq = Arc::new(Line::default());
        (Smbus::new("ich9-smbus", irq.clone()), irq)
    }

    fn enable(device: &mut Smbus) {
        device.config_write(1, 0, &[3, 1]);
        device.config_write(0x40 / 4, 0, &[1]);
    }

    #[test]
    fn firmware_sizes_allocates_and_enables_bar() {
        let (mut device, _) = controller();
        assert_eq!(device.config_read(0), 0x2930_8086);
        assert_eq!(device.config_read(2), 0x0c05_0002);
        assert_eq!(device.config_read(0x2c / 4), 0x1100_1af4);
        device.config_write(8, 0, &u32::MAX.to_le_bytes());
        assert_eq!(device.config_read(8), 0xffff_ffc1);
        device.config_write(8, 0, &0x701u32.to_le_bytes());
        assert_eq!(device.io_base(), 0x700);
        assert!(!device.enabled());
        enable(&mut device);
        assert!(device.enabled());
        assert_eq!(device.config_read(1), 0x103);
        device.config_write(0, 0, &[0; 4]);
        assert_eq!(device.config_read(0), 0x2930_8086);
    }

    #[test]
    fn no_slave_transactions_report_nack_and_clear_interrupt() {
        let (mut device, irq) = controller();
        enable(&mut device);
        let mut status = [0];
        for address in 0..128u8 {
            device.bar_write(4, 0, &[STATUS_W1C]);
            device.bar_write(4, 4, &[address << 1]);
            device.bar_write(4, 2, &[0x41]);
            device.bar_read(4, 0, &mut status);
            assert_eq!(status, [DEV_ERR]);
            assert!(irq.0.load(Ordering::SeqCst));
            device.bar_read(4, 2, &mut status);
            assert_eq!(status, [1]);
            device.bar_write(4, 0, &[DEV_ERR]);
            assert!(!irq.0.load(Ordering::SeqCst));
        }
    }

    #[test]
    fn disabled_host_and_pci_interrupt_disable() {
        let (mut device, irq) = controller();
        device.config_write(1, 0, &[1]);
        device.bar_write(4, 2, &[0x41]);
        let mut status = [0];
        device.bar_read(4, 0, &mut status);
        assert_eq!(status, [0]);
        enable(&mut device);
        device.config_write(1, 1, &[4]);
        device.bar_write(4, 2, &[0x41]);
        assert!(!irq.0.load(Ordering::SeqCst));
        device.config_write(1, 1, &[0]);
        assert!(irq.0.load(Ordering::SeqCst));
    }

    #[test]
    fn every_transfer_protocol_reports_no_slave() {
        let (mut device, irq) = controller();
        enable(&mut device);
        for protocol in 0..8u8 {
            for read in [false, true] {
                device.bar_write(4, 0, &[STATUS_W1C]);
                device.bar_write(4, 3, &[0x19]);
                device.bar_write(4, 4, &[0xa0 | u8::from(read)]);
                device.bar_write(4, 5, &[2, 0xaa, 0x55]);
                device.bar_write(4, 2, &[0x41 | protocol << 2]);
                let mut status = [0];
                device.bar_read(4, 0, &mut status);
                assert_eq!(status, [DEV_ERR]);
                assert!(irq.0.load(Ordering::SeqCst));
            }
        }
    }

    #[test]
    fn invalid_accesses_leave_registers_unchanged() {
        let (mut device, _) = controller();
        enable(&mut device);
        let original = device.save_state().unwrap();
        device.config_write(usize::MAX, 0, &[0xff]);
        device.config_write(0, u64::MAX, &[0xff]);
        device.config_write(0, 3, &[0xff; 4]);
        device.config_write(64, 0, &[0xff; 4]);
        device.write(u64::MAX, &[0xff; 4]);
        device.bar_write(0, 2, &[0x41]);
        assert_eq!(device.config_read(usize::MAX), u32::MAX);
        assert_eq!(device.config_read(64), u32::MAX);
        let mut data = [0; 4];
        device.read(u64::MAX, &mut data);
        assert_eq!(data, [0xff; 4]);
        assert_eq!(device.save_state().unwrap(), original);
    }

    #[test]
    fn snapshot_restores_pending_interrupt_and_rejects_truncation() {
        let (mut device, irq) = controller();
        enable(&mut device);
        device.bar_write(4, 2, &[0x41]);
        let state = device.save_state().unwrap();
        device.bar_write(4, 0, &[DEV_ERR]);
        assert!(!irq.0.load(Ordering::SeqCst));
        device.restore_state(&state).unwrap();
        assert!(irq.0.load(Ordering::SeqCst));
        for len in 0..state.len() {
            assert!(device.restore_state(&state[..len]).is_err());
        }
    }
}
