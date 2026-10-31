//! KVM registrations for the virtio-gpu host-visible shared-memory BAR.

use kvm_bindings::kvm_userspace_memory_region;
use kvm_ioctls::VmFd;
use parking_lot::Mutex;
use std::collections::BTreeMap;
use std::sync::Arc;
use vmlab_hv_devices::{
    DeviceError,
    virtio::gpu::{APERTURE_SIZE, BlobMapper},
};

struct Mapping {
    slot: u32,
    offset: u64,
    host: u64,
    size: u64,
}

#[derive(Default)]
struct State {
    base: Option<u64>,
    mappings: BTreeMap<u32, Mapping>,
}

pub(crate) struct Aperture {
    vm: Arc<VmFd>,
    state: Mutex<State>,
    slots: u32,
}

impl Aperture {
    pub(crate) fn new(vm: Arc<VmFd>, slots: u32) -> Self {
        Self {
            vm,
            state: Mutex::new(State::default()),
            slots,
        }
    }

    fn register(&self, mapping: &Mapping, base: Option<u64>) -> Result<(), DeviceError> {
        for space in [0, 1 << 16] {
            let region = kvm_userspace_memory_region {
                slot: space | mapping.slot,
                guest_phys_addr: base.map_or(0, |base| base + mapping.offset),
                memory_size: if base.is_some() { mapping.size } else { 0 },
                userspace_addr: if base.is_some() { mapping.host } else { 0 },
                flags: 0,
            };
            // SAFETY: the renderer retains the mapping until both registrations
            // are removed. KVM invalidates secondary MMUs before returning.
            unsafe { self.vm.set_user_memory_region(region) }
                .map_err(|e| DeviceError::Other(format!("GPU blob KVM mapping: {e}")))?;
        }
        Ok(())
    }
}

impl BlobMapper for Aperture {
    fn set_base(&self, base: Option<u64>) -> Result<(), DeviceError> {
        if base.is_some_and(|b| {
            b < APERTURE_SIZE || b % APERTURE_SIZE != 0 || b.checked_add(APERTURE_SIZE).is_none()
        }) {
            return Err(DeviceError::Other("invalid GPU blob BAR address".into()));
        }
        let mut state = self.state.lock();
        if state.base == base {
            return Ok(());
        }
        for mapping in state.mappings.values() {
            if state.base.is_some() {
                self.register(mapping, None)?;
            }
            if base.is_some() {
                self.register(mapping, base)?;
            }
        }
        state.base = base;
        Ok(())
    }

    fn map(&self, id: u32, offset: u64, host: u64, size: u64) -> Result<(), DeviceError> {
        let mut state = self.state.lock();
        if state.base.is_none()
            || state.mappings.contains_key(&id)
            || host == 0
            || (host | offset | size) & 4095 != 0
            || size == 0
            || offset
                .checked_add(size)
                .is_none_or(|end| end > APERTURE_SIZE)
            || state
                .mappings
                .values()
                .any(|m| offset < m.offset + m.size && m.offset < offset + size)
        {
            return Err(DeviceError::Other(
                "invalid or overlapping GPU blob mapping".into(),
            ));
        }
        let slot = (129..self.slots)
            .find(|slot| state.mappings.values().all(|m| m.slot != *slot))
            .ok_or_else(|| DeviceError::Other("GPU blob KVM memory slots exhausted".into()))?;
        let mapping = Mapping {
            slot,
            offset,
            host,
            size,
        };
        if let Err(error) = self.register(&mapping, state.base) {
            // Roll back a possible first-address-space registration.
            self.register(&mapping, None)?;
            return Err(error);
        }
        state.mappings.insert(id, mapping);
        Ok(())
    }

    fn unmap(&self, id: u32) -> Result<(), DeviceError> {
        let mut state = self.state.lock();
        let mapping = state
            .mappings
            .get(&id)
            .ok_or_else(|| DeviceError::Other("unknown GPU blob mapping".into()))?;
        if state.base.is_some() {
            self.register(mapping, None)?;
        }
        state.mappings.remove(&id);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use vm_memory::{GuestAddress, GuestMemoryBackend};

    #[test]
    fn blob_slots_map_relocate_reject_overlap_and_unmap() {
        if !std::path::Path::new("/dev/kvm").exists() {
            eprintln!("SKIP GPU blob registration: /dev/kvm is absent");
            return;
        }
        let kvm = crate::kvm::KvmVm::new().unwrap();
        let memory =
            vmlab_hv_devices::GuestMemory::from_ranges(&[(GuestAddress(0), 8192)]).unwrap();
        let host = memory.get_host_address(GuestAddress(0)).unwrap() as u64;
        let aperture = Aperture::new(kvm.vm.clone(), kvm.kvm.get_nr_memslots() as u32);
        assert!(aperture.map(1, 0, host, 4096).is_err());
        assert!(aperture.set_base(Some(4096)).is_err());
        aperture.set_base(Some(1 << 32)).unwrap();
        aperture.map(1, 0, host, 4096).unwrap();
        assert!(aperture.map(2, 0, host + 4096, 4096).is_err());
        assert!(aperture.map(2, APERTURE_SIZE, host + 4096, 4096).is_err());
        aperture.map(2, 4096, host + 4096, 4096).unwrap();
        aperture.set_base(Some(2 << 32)).unwrap();
        aperture.set_base(None).unwrap();
        aperture.set_base(Some(3 << 32)).unwrap();
        aperture.unmap(1).unwrap();
        aperture.unmap(2).unwrap();
        assert!(aperture.unmap(2).is_err());
    }
}
