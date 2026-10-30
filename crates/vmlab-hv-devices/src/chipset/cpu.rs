//! Fixed CPU inventory at the Q35 ACPI CPU-register interface.
//!
//! Register layout: https://www.qemu.org/docs/master/specs/acpi_cpu_hotplug.html
//! All possible CPUs are present; this machine exposes no CPU hotplug capability.

use crate::{BusDevice, DeviceError, Snapshot};

pub struct CpuInventory {
    count: u32,
    selector: u32,
    command: u8,
    ost: Vec<[u32; 2]>,
}

impl CpuInventory {
    pub fn new(count: u32) -> Self {
        assert!(count > 0);
        Self {
            count,
            selector: 0,
            command: 0,
            ost: vec![[0; 2]; count as usize],
        }
    }
}

impl BusDevice for CpuInventory {
    fn read(&mut self, offset: u64, data: &mut [u8]) {
        data.fill(0);
        if self.selector >= self.count {
            return;
        }
        let value = match (offset, data.len()) {
            (4, 1) => 1,
            (8, 4) if self.command == 0 || self.command == 3 => self.selector,
            _ => 0,
        };
        let bytes = value.to_le_bytes();
        if data.len() <= bytes.len() {
            data.copy_from_slice(&bytes[..data.len()]);
        }
    }
    fn write(&mut self, offset: u64, data: &[u8]) {
        if offset == 0 && data.len() == 4 {
            self.selector = u32::from_le_bytes(data.try_into().unwrap());
        } else if self.selector < self.count {
            match (offset, data.len()) {
                (5, 1) => self.command = data[0],
                (8, 4) if matches!(self.command, 1 | 2) => {
                    self.ost[self.selector as usize][self.command as usize - 1] =
                        u32::from_le_bytes(data.try_into().unwrap());
                }
                _ => {}
            }
        }
    }
}

impl Snapshot for CpuInventory {
    fn state_id(&self) -> String {
        "q35-cpu-inventory".into()
    }
    fn save_state(&self) -> Result<Vec<u8>, DeviceError> {
        let mut state = vec![1, self.command];
        state.extend(self.count.to_le_bytes());
        state.extend(self.selector.to_le_bytes());
        for ost in &self.ost {
            for value in ost {
                state.extend(value.to_le_bytes());
            }
        }
        Ok(state)
    }
    fn restore_state(&mut self, state: &[u8]) -> Result<(), DeviceError> {
        if state.len() != 10 + self.count as usize * 8
            || state[0] != 1
            || u32::from_le_bytes(state[2..6].try_into().unwrap()) != self.count
        {
            return Err(DeviceError::State("invalid CPU inventory state".into()));
        }
        self.command = state[1];
        self.selector = u32::from_le_bytes(state[6..10].try_into().unwrap());
        for (ost, bytes) in self.ost.iter_mut().zip(state[10..].chunks_exact(8)) {
            ost[0] = u32::from_le_bytes(bytes[..4].try_into().unwrap());
            ost[1] = u32::from_le_bytes(bytes[4..].try_into().unwrap());
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn firmware_enumerates_all_present_apic_ids_and_stops_at_the_limit() {
        let mut cpu = CpuInventory::new(4);
        for id in 0u32..=4 {
            cpu.write(0, &id.to_le_bytes());
            cpu.write(5, &[3]);
            let mut enabled = [0];
            cpu.read(4, &mut enabled);
            assert_eq!(enabled, [u8::from(id < 4)]);
            let mut data = [0; 4];
            cpu.read(8, &mut data);
            assert_eq!(u32::from_le_bytes(data), if id < 4 { id } else { 0 });
            cpu.read(0, &mut data);
            assert_eq!(data, [0; 4]);
        }
        cpu.write(0, &2u32.to_le_bytes());
        cpu.write(5, &[0]);
        let mut data = [0; 4];
        cpu.read(8, &mut data);
        assert_eq!(u32::from_le_bytes(data), 2);
        let saved = cpu.save_state().unwrap();
        cpu.write(0, &[0; 4]);
        cpu.restore_state(&saved).unwrap();
        cpu.read(8, &mut data);
        assert_eq!(u32::from_le_bytes(data), 2);
    }
}
