//! CPU-only A20 aliases over memfd-backed RAM. DMA retains the physical view.

use crate::{error::VmmError, q35::Pam};
use kvm_bindings::kvm_userspace_memory_region;
use kvm_ioctls::VmFd;
use parking_lot::Mutex;
use std::sync::Arc;
use vm_memory::{Bytes, GuestAddress, GuestMemoryBackend, GuestMemoryMmap, GuestMemoryRegion};
use vmlab_hv_devices::{DeviceError, Snapshot};

const MIB: u64 = 1 << 20;
const APERTURE_SLOT: u32 = 200;
const UPPER_SLOT: u32 = 201;

pub struct A20 {
    file: std::fs::File,
    cpu: GuestMemoryMmap,
    vm: Arc<VmFd>,
    regions: Vec<(u64, u64)>,
    enabled: bool,
    pam: Option<Arc<Mutex<Pam>>>,
    rom: Option<(GuestMemoryMmap, GuestMemoryMmap, u32)>,
}

impl A20 {
    pub fn new(
        file: std::fs::File,
        vm: Arc<VmFd>,
        regions: &[(u64, u64)],
    ) -> Result<Self, VmmError> {
        let ranges = regions
            .iter()
            .map(|&(base, size)| {
                Ok((
                    GuestAddress(base),
                    size as usize,
                    Some(vm_memory::FileOffset::new(file.try_clone()?, base)),
                ))
            })
            .collect::<Result<Vec<_>, std::io::Error>>()
            .map_err(|e| VmmError::Memory(e.to_string()))?;
        let cpu = GuestMemoryMmap::from_ranges_with_files(ranges)
            .map_err(|e| VmmError::Memory(e.to_string()))?;
        crate::memory::dont_dump(&cpu)?;
        Ok(Self {
            file,
            cpu,
            vm,
            regions: regions.to_vec(),
            enabled: true,
            pam: None,
            rom: None,
        })
    }

    pub fn attach_pam(&mut self, pam: Arc<Mutex<Pam>>) {
        self.pam = Some(pam);
    }
    pub fn attach_rom(&mut self, rom: GuestMemoryMmap, slot: u32) -> Result<(), VmmError> {
        let region = rom.iter().next().unwrap();
        let base = region.start_addr().0;
        let alias = GuestMemoryMmap::from_ranges(&[(GuestAddress(base), region.len() as usize)])
            .map_err(|e| VmmError::Memory(e.to_string()))?;
        crate::memory::dont_dump(&alias)?;
        let mut bytes = vec![0xff; region.len() as usize];
        let mut offset = 0;
        while offset < bytes.len() {
            let addr = base + offset as u64;
            let len = (MIB - (addr & (MIB - 1))).min(bytes.len() as u64 - offset as u64) as usize;
            let source = addr & !MIB;
            let start = base.saturating_sub(source).min(len as u64) as usize;
            let end = (base + region.len()).saturating_sub(source).min(len as u64) as usize;
            if start < end {
                rom.read_slice(
                    &mut bytes[offset + start..offset + end],
                    GuestAddress(source + start as u64),
                )
                .map_err(|e| VmmError::Memory(e.to_string()))?;
            }
            offset += len;
        }
        alias
            .write_slice(&bytes, GuestAddress(base))
            .map_err(|e| VmmError::Memory(e.to_string()))?;
        self.rom = Some((rom, alias, slot));
        Ok(())
    }
    pub fn enabled(&self) -> bool {
        self.enabled
    }

    pub fn register(&self) -> Result<(), VmmError> {
        for (slot, &(base, size)) in self.regions.iter().enumerate() {
            if base == MIB {
                self.map(slot as u32, base, 0xa0000)?;
                if self.enabled {
                    self.map(APERTURE_SLOT, 0x1a0000, 0x60000)?;
                }
                self.map(UPPER_SLOT, 2 * MIB, size - MIB)?;
            } else {
                self.map(slot as u32, base, size)?;
            }
        }
        Ok(())
    }

    pub fn invalidate(&self) -> Result<(), VmmError> {
        for slot in (0..self.regions.len() as u32)
            .chain([UPPER_SLOT])
            .chain(self.enabled.then_some(APERTURE_SLOT))
        {
            // SAFETY: vCPUs are parked and the backing mappings stay retained.
            unsafe {
                self.vm.set_user_memory_region(kvm_userspace_memory_region {
                    slot,
                    ..Default::default()
                })
            }
            .map_err(|e| VmmError::ioctl("invalidating A20 RAM", e))?;
        }
        Ok(())
    }

    fn map_rom(&self, enabled: bool) -> Result<(), VmmError> {
        let Some((rom, alias, slot)) = &self.rom else {
            return Ok(());
        };
        let region = if enabled { rom } else { alias }.iter().next().unwrap();
        // SAFETY: vCPUs are parked and both backings remain retained. KVM
        // requires removal before the userspace address of a slot changes.
        unsafe {
            self.vm.set_user_memory_region(kvm_userspace_memory_region {
                slot: *slot,
                ..Default::default()
            })
        }
        .map_err(|e| VmmError::ioctl("removing A20 BIOS", e))?;
        // SAFETY: both immutable mappings remain owned by this manager, and
        // all vCPUs are parked while the normal address-space slot changes.
        unsafe {
            self.vm.set_user_memory_region(kvm_userspace_memory_region {
                slot: *slot,
                guest_phys_addr: region.start_addr().0,
                memory_size: region.len(),
                userspace_addr: region.as_ptr() as u64,
                flags: kvm_bindings::KVM_MEM_READONLY,
            })
        }
        .map_err(|e| VmmError::ioctl("mapping A20 BIOS", e))
    }

    fn map(&self, slot: u32, base: u64, size: u64) -> Result<(), VmmError> {
        let region = kvm_userspace_memory_region {
            slot,
            guest_phys_addr: base,
            memory_size: size,
            userspace_addr: if size == 0 {
                0
            } else {
                self.cpu
                    .get_host_address(GuestAddress(base))
                    .map_err(|e| VmmError::Memory(e.to_string()))? as u64
            },
            flags: 0,
        };
        // SAFETY: the CPU mapping is page aligned and owned until the VM stops.
        unsafe { self.vm.set_user_memory_region(region) }
            .map_err(|e| VmmError::ioctl("KVM_SET_USER_MEMORY_REGION(A20)", e))
    }

    /// The caller parks all vCPUs before changing the CPU address space.
    pub fn set_enabled(&mut self, enabled: bool) -> Result<(), VmmError> {
        use std::os::fd::AsRawFd;
        if self.enabled == enabled {
            return Ok(());
        }
        self.map_rom(enabled)?;
        if !enabled {
            self.map(APERTURE_SLOT, 0x1a0000, 0)?;
        }
        if let Some(pam) = &self.pam {
            pam.lock().set_a20(enabled)?;
        }
        for &(base, size) in &self.regions {
            let end = base + size;
            let mut addr = (base / MIB) * MIB;
            while addr < end {
                if addr & MIB != 0 {
                    let start = addr.max(base);
                    let len = (addr + MIB).min(end) - start;
                    let pointer = self
                        .cpu
                        .get_host_address(GuestAddress(start))
                        .map_err(|e| VmmError::Memory(e.to_string()))?;
                    let offset = if enabled { start } else { start & !MIB };
                    // SAFETY: MAP_FIXED replaces only pages inside our own
                    // live CPU mapping with aliases of our retained memfd.
                    let result = unsafe {
                        libc::mmap(
                            pointer.cast(),
                            len as usize,
                            libc::PROT_READ | libc::PROT_WRITE,
                            libc::MAP_SHARED | libc::MAP_FIXED,
                            self.file.as_raw_fd(),
                            offset as libc::off_t,
                        )
                    };
                    if result == libc::MAP_FAILED {
                        return Err(VmmError::Memory(format!(
                            "A20 alias mmap: {}",
                            std::io::Error::last_os_error()
                        )));
                    }
                }
                addr += MIB;
            }
        }
        // MAP_FIXED replaces VMA advice along with the mapping.
        crate::memory::dont_dump(&self.cpu)?;
        self.enabled = enabled;
        if enabled {
            self.map(APERTURE_SLOT, 0x1a0000, 0x60000)?;
        }
        Ok(())
    }
}

impl Snapshot for A20 {
    fn state_id(&self) -> String {
        "pc-a20".into()
    }
    fn save_state(&self) -> Result<Vec<u8>, DeviceError> {
        Ok(vec![1, u8::from(self.enabled)])
    }
    fn restore_state(&mut self, state: &[u8]) -> Result<(), DeviceError> {
        if state.len() != 2 || state[0] != 1 || state[1] > 1 {
            return Err(DeviceError::State("invalid A20 state".into()));
        }
        self.set_enabled(state[1] != 0)
            .map_err(|e| DeviceError::State(e.to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use vm_memory::Bytes;

    #[test]
    #[cfg_attr(vmlab_ci_no_kvm, ignore = "requires KVM and/or local guest assets")]
    fn bios_alias_retains_the_mapped_suffix_of_a_partial_megabyte() {
        if !std::path::Path::new("/dev/kvm").exists() {
            eprintln!("skipping BIOS alias test: /dev/kvm is unavailable");
            return;
        }
        let kvm = crate::kvm::KvmVm::new().unwrap();
        let ram =
            crate::memory::GuestRam::pc(crate::layout::MemLayout::pc(8).unwrap(), kvm.vm.clone())
                .unwrap();
        let base = 0xffe8_0000;
        let rom = GuestMemoryMmap::from_ranges(&[(GuestAddress(base), 0x180000)]).unwrap();
        rom.write_slice(&vec![0x5a; 0x180000], GuestAddress(base))
            .unwrap();
        let mut a20 = ram.a20.as_ref().unwrap().lock();
        a20.attach_rom(rom, 2).unwrap();
        let alias = &a20.rom.as_ref().unwrap().1;
        for (addr, expected) in [
            (0xffe80000, 0x5a),
            (0xfff00000, 0xff),
            (0xfff7ffff, 0xff),
            (0xfff80000, 0x5a),
            (0xffffffff, 0x5a),
        ] {
            assert_eq!(
                alias.read_obj::<u8>(GuestAddress(addr)).unwrap(),
                expected,
                "address {addr:#x}"
            );
        }
    }

    #[test]
    #[cfg_attr(vmlab_ci_no_kvm, ignore = "requires KVM and/or local guest assets")]
    fn cpu_aliases_each_odd_megabyte_without_aliasing_dma_or_losing_ram() {
        if !std::path::Path::new("/dev/kvm").exists() {
            eprintln!("skipping A20 mapping test: /dev/kvm is unavailable");
            return;
        }
        let kvm = crate::kvm::KvmVm::new().unwrap();
        let ram =
            crate::memory::GuestRam::pc(crate::layout::MemLayout::pc(8).unwrap(), kvm.vm.clone())
                .unwrap();
        crate::memory::register_memslots(&kvm.vm, &ram).unwrap();
        let mut a20 = ram.a20.as_ref().unwrap().lock();
        for bank in 0..8 {
            ram.mem
                .write_slice(&[bank as u8], GuestAddress(bank * MIB + 0x500))
                .unwrap();
        }
        a20.set_enabled(false).unwrap();
        let disabled = a20.save_state().unwrap();
        for bank in 0..8 {
            let mut byte = [0];
            a20.cpu
                .read_slice(&mut byte, GuestAddress(bank * MIB + 0x500))
                .unwrap();
            assert_eq!(byte, [bank as u8 & !1]);
            ram.mem
                .read_slice(&mut byte, GuestAddress(bank * MIB + 0x500))
                .unwrap();
            assert_eq!(byte, [bank as u8]);
        }
        a20.cpu
            .write_slice(&[0x55], GuestAddress(3 * MIB + 0x500))
            .unwrap();
        let mut byte = [0];
        ram.mem
            .read_slice(&mut byte, GuestAddress(2 * MIB + 0x500))
            .unwrap();
        assert_eq!(byte, [0x55]);
        a20.set_enabled(true).unwrap();
        a20.cpu
            .read_slice(&mut byte, GuestAddress(3 * MIB + 0x500))
            .unwrap();
        assert_eq!(byte, [3]);
        a20.restore_state(&disabled).unwrap();
        a20.cpu
            .read_slice(&mut byte, GuestAddress(3 * MIB + 0x500))
            .unwrap();
        assert_eq!(byte, [0x55]);
        assert!(a20.restore_state(&[1, 2]).is_err());
    }
}
