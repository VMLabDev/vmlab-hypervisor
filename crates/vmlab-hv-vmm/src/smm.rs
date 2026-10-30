//! Q35 SMM integration primitives. Reserve SMRAM ranges outside ordinary RAM
//! memslots and DMA views, mirror ordinary RAM in address space 1, then install
//! `Smram` for the ranges reported by `SmramControl`. Stop all vCPUs while
//! changing mappings. Retain `Smram` until every vCPU has stopped.

use std::sync::Arc;

use kvm_bindings::{kvm_userspace_memory_region, kvm_vcpu_events};
use kvm_ioctls::{Cap, Kvm, VcpuFd, VmFd};
use vm_memory::{
    Address, Bytes, GuestAddress, GuestMemoryBackend, GuestMemoryMmap, GuestMemoryRegion,
};
use vmlab_hv_devices::{BusDevice, DeviceError, Snapshot};

use crate::VmmError;

pub const SMM_ADDRESS_SPACE: u32 = 1 << 16;
pub const SMI_BROADCAST: u64 = 1;

mod fwcfg;
pub use fwcfg::SmiFwCfg;

pub fn require_smm(kvm: &Kvm) -> Result<(), VmmError> {
    if !kvm.check_extension(Cap::X86Smm) {
        return Err(VmmError::KvmCap("KVM_CAP_X86_SMM"));
    }
    Ok(())
}

/// Query on the vCPU that issued an MMIO write, before authorizing pflash.
pub fn in_smm(vcpu: &VcpuFd) -> Result<bool, VmmError> {
    vcpu.get_vcpu_events()
        .map(|e| e.smi.smm != 0)
        .map_err(|e| VmmError::ioctl("KVM_GET_VCPU_EVENTS(SMM)", e))
}

/// Preserve all event fields, including pending/nested SMI and NMI masking.
pub fn save_vcpu_events(vcpu: &VcpuFd) -> Result<kvm_vcpu_events, VmmError> {
    vcpu.get_vcpu_events()
        .map_err(|e| VmmError::ioctl("KVM_GET_VCPU_EVENTS", e))
}

pub fn restore_vcpu_events(vcpu: &VcpuFd, events: &kvm_vcpu_events) -> Result<(), VmmError> {
    vcpu.set_vcpu_events(events)
        .map_err(|e| VmmError::ioctl("KVM_SET_VCPU_EVENTS", e))
}

/// Call for every vCPU when broadcast was negotiated, otherwise only the BSP.
pub fn inject_smi(vcpu: &VcpuFd) -> Result<(), VmmError> {
    vcpu.smi().map_err(|e| VmmError::ioctl("KVM_SMI", e))
}

/// MCH SMRAMC/ESMRAMC policy. TSEGMB is derived, read-only, and MiB aligned.
#[derive(Clone)]
pub struct SmramControl {
    smramc: u8,
    esmramc: u8,
    low_ram_end: u64,
    extended_mb: u16,
}

impl SmramControl {
    pub fn new(low_ram_end: u64, extended_mb: u16) -> Result<Self, VmmError> {
        let max = u64::from(extended_mb).max(8) << 20;
        if extended_mb == 0
            || !low_ram_end.is_multiple_of(1 << 20)
            || low_ram_end < max
            || low_ram_end > 1 << 32
        {
            return Err(VmmError::InvalidMachine("invalid TSEG geometry".into()));
        }
        Ok(Self {
            smramc: 2,
            esmramc: 0x38,
            low_ram_end,
            extended_mb,
        })
    }

    pub fn read_config(&self, offset: usize) -> Option<u8> {
        match offset {
            0x50..=0x51 => Some(self.extended_mb.to_le_bytes()[offset - 0x50]),
            0x9d => Some(self.smramc),
            0x9e => Some(self.esmramc),
            0xac..=0xaf => Some((self.tseg_base() as u32).to_le_bytes()[offset - 0xac]),
            _ => None,
        }
    }

    /// Returns true for an owned config byte, including read-only registers.
    pub fn write_config(&mut self, offset: usize, value: u8) -> bool {
        match offset {
            0x9d if self.smramc & 0x10 == 0 => {
                self.smramc = (value & 0x78) | 2;
                if self.smramc & 0x10 != 0 {
                    self.smramc &= !0x40;
                }
            }
            0x9e if self.smramc & 0x10 == 0 => self.esmramc = 0x38 | (value & 7),
            0x50..=0x51 | 0x9d..=0x9e | 0xac..=0xaf => {}
            _ => return false,
        }
        true
    }

    pub fn tseg_size(&self) -> u64 {
        if self.esmramc & 1 == 0 {
            return 0;
        }
        match self.esmramc & 6 {
            0 => 1 << 20,
            2 => 2 << 20,
            4 => 8 << 20,
            _ => u64::from(self.extended_mb) << 20,
        }
    }
    pub fn tseg_base(&self) -> u64 {
        self.low_ram_end - self.tseg_size()
    }
    pub fn open(&self) -> bool {
        self.smramc & 0x78 == 0x48
    }
    pub fn enabled(&self) -> bool {
        self.smramc & 8 != 0
    }

    /// Legacy SMRAM and enabled TSEG. The default SMBASE save area at 0x30000
    /// remains in mirrored RAM until firmware relocates it.
    pub fn ranges(&self) -> Vec<(u64, usize)> {
        if !self.enabled() {
            return Vec::new();
        }
        let mut ranges = vec![(0xa0000, 0x20000)];
        if self.tseg_size() != 0 {
            ranges.push((self.tseg_base(), self.tseg_size() as usize));
        }
        ranges
    }
}

impl Snapshot for SmramControl {
    fn state_id(&self) -> String {
        "smm-mch".into()
    }
    fn save_state(&self) -> Result<Vec<u8>, DeviceError> {
        Ok(vec![1, self.smramc, self.esmramc])
    }
    fn restore_state(&mut self, state: &[u8]) -> Result<(), DeviceError> {
        if state.len() != 3
            || state[0] != 1
            || state[1] & !0x78 != 2
            || state[2] & !7 != 0x38
            || state[1] & 0x50 == 0x50
        {
            return Err(DeviceError::State("invalid SMRAM controls".into()));
        }
        self.smramc = state[1];
        self.esmramc = state[2];
        Ok(())
    }
}

/// Owns isolated backing and both KVM aliases. Slot IDs must be reserved in
/// both address spaces. Ordinary RAM must not overlap these ranges.
pub struct Smram {
    vm: Arc<VmFd>,
    memory: GuestMemoryMmap,
    first_slot: u32,
    open: bool,
}

impl Smram {
    pub fn new(vm: Arc<VmFd>, ranges: &[(u64, usize)], first_slot: u32) -> Result<Self, VmmError> {
        if ranges.is_empty()
            || first_slot as usize + ranges.len() > 65536
            || ranges.iter().any(|(base, size)| {
                !base.is_multiple_of(4096) || *size == 0 || !size.is_multiple_of(4096)
            })
        {
            return Err(VmmError::InvalidMachine(
                "invalid SMRAM ranges or slots".into(),
            ));
        }
        let ranges: Vec<_> = ranges
            .iter()
            .map(|(base, size)| (GuestAddress(*base), *size))
            .collect();
        let memory =
            GuestMemoryMmap::from_ranges(&ranges).map_err(|e| VmmError::Memory(e.to_string()))?;
        let smram = Self {
            vm,
            memory,
            first_slot,
            open: false,
        };
        smram.map(SMM_ADDRESS_SPACE, true)?;
        Ok(smram)
    }

    fn map(&self, address_space: u32, present: bool) -> Result<(), VmmError> {
        for (index, region) in self.memory.iter().enumerate() {
            let slot = kvm_userspace_memory_region {
                slot: address_space | (self.first_slot + index as u32),
                guest_phys_addr: region.start_addr().raw_value(),
                memory_size: if present { region.len() } else { 0 },
                userspace_addr: region.as_ptr() as u64,
                flags: 0,
            };
            // SAFETY: page-aligned backing is owned here and unregistered on
            // drop, before its allocation is released. vCPUs must be stopped.
            unsafe { self.vm.set_user_memory_region(slot) }
                .map_err(|e| VmmError::ioctl("KVM_SET_USER_MEMORY_REGION(SMRAM)", e))?;
        }
        Ok(())
    }

    pub fn set_open(&mut self, open: bool) -> Result<(), VmmError> {
        if self.open != open {
            self.map(0, open)?;
            self.open = open;
        }
        Ok(())
    }

    /// Host-only backing access for loading handlers and snapshot orchestration.
    /// Never merge this memory into a bus-master device's DMA memory view.
    pub fn memory(&self) -> &GuestMemoryMmap {
        &self.memory
    }
}

impl Drop for Smram {
    fn drop(&mut self) {
        // Keep backing alive on an unexpected unmap failure: dangling KVM
        // memslots would otherwise allow a vCPU to access freed host memory.
        if self
            .map(0, false)
            .and_then(|()| self.map(SMM_ADDRESS_SPACE, false))
            .is_err()
        {
            std::mem::forget(self.memory.clone());
        }
    }
}

impl Snapshot for Smram {
    fn state_id(&self) -> String {
        "smm-ram".into()
    }
    fn save_state(&self) -> Result<Vec<u8>, DeviceError> {
        let mut state = vec![1, self.open as u8];
        for region in self.memory.iter() {
            state.extend(region.start_addr().raw_value().to_le_bytes());
            state.extend(region.len().to_le_bytes());
            let mut data = vec![0; region.len() as usize];
            region
                .read_slice(&mut data, vm_memory::MemoryRegionAddress(0))
                .map_err(|e| DeviceError::Memory(e.to_string()))?;
            state.extend(data);
        }
        Ok(state)
    }
    fn restore_state(&mut self, state: &[u8]) -> Result<(), DeviceError> {
        let invalid = || DeviceError::State("invalid SMRAM state".into());
        if state.len() < 2 || state[0] != 1 || state[1] > 1 {
            return Err(invalid());
        }
        let mut rest = &state[2..];
        let mut buffers = Vec::new();
        for region in self.memory.iter() {
            let header = rest.get(..16).ok_or_else(invalid)?;
            if u64::from_le_bytes(header[..8].try_into().unwrap())
                != region.start_addr().raw_value()
                || u64::from_le_bytes(header[8..].try_into().unwrap()) != region.len()
            {
                return Err(invalid());
            }
            let end = 16usize
                .checked_add(region.len() as usize)
                .ok_or_else(invalid)?;
            buffers.push(rest.get(16..end).ok_or_else(invalid)?);
            rest = &rest[end..];
        }
        if !rest.is_empty() {
            return Err(invalid());
        }
        self.set_open(state[1] != 0)
            .map_err(|e| DeviceError::State(e.to_string()))?;
        for (region, data) in self.memory.iter().zip(buffers) {
            region
                .write_slice(data, vm_memory::MemoryRegionAddress(0))
                .map_err(|e| DeviceError::Memory(e.to_string()))?;
        }
        Ok(())
    }
}

/// ICH9 APM SMI source. Route PMBASE+0x30/+0x34 and APMC/APMS here;
/// route LPC GEN_PMCON_1 bit 4 to `lock` after each PCI configuration write.
#[derive(Default)]
pub struct SmiControl {
    enable: u32,
    status: u32,
    locked: bool,
    apmc: u8,
    apms: u8,
}

impl SmiControl {
    pub fn lock(&mut self) {
        self.locked = true;
    }
    /// Register offsets 0x30/0x34 are relative to PMBASE, 0xb2/0xb3 absolute.
    pub fn read(&self, register: u16) -> Option<u32> {
        match register {
            0x30 => Some(self.enable),
            0x34 => Some(self.status),
            0xb2 => Some(self.apmc as u32),
            0xb3 => Some(self.apms as u32),
            _ => None,
        }
    }
    /// True means inject an SMI (broadcast if negotiated). APMC is an edge
    /// source; EOS is retained but does not suppress subsequent APMC writes.
    pub fn write(&mut self, register: u16, value: u32) -> bool {
        match register {
            0x30 => {
                self.enable = (value & 0x23 & !u32::from(self.locked))
                    | (self.enable & u32::from(self.locked))
            }
            0x34 => self.status &= !(value & (1 << 5)),
            0xb3 => self.apms = value as u8,
            0xb2 => {
                self.apmc = value as u8;
                if self.enable & 0x21 == 0x21 {
                    self.status |= 1 << 5;
                    return true;
                }
            }
            _ => {}
        }
        false
    }
}

impl Snapshot for SmiControl {
    fn state_id(&self) -> String {
        "smm-smi".into()
    }
    fn save_state(&self) -> Result<Vec<u8>, DeviceError> {
        let mut state = vec![1, self.locked as u8, self.apmc, self.apms];
        state.extend(self.enable.to_le_bytes());
        state.extend(self.status.to_le_bytes());
        Ok(state)
    }
    fn restore_state(&mut self, state: &[u8]) -> Result<(), DeviceError> {
        if state.len() != 12 || state[0] != 1 || state[1] > 1 {
            return Err(DeviceError::State("invalid SMI state".into()));
        }
        self.locked = state[1] != 0;
        self.apmc = state[2];
        self.apms = state[3];
        self.enable = u32::from_le_bytes(state[4..8].try_into().unwrap());
        self.status = u32::from_le_bytes(state[8..12].try_into().unwrap());
        Ok(())
    }
}

#[cfg(test)]
mod tests;
