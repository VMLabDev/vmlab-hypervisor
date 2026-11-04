//! KVM memory and doorbell registrations for the shared-memory monitor.

use kvm_ioctls::{IoEventAddress, NoDatamatch, VmFd};
use std::io;
use std::sync::Arc;
use vmlab_hv_devices::vdisplay::{Mapper, Region};
use vmm_sys_util::eventfd::EventFd;

pub(crate) struct Mapping {
    vm: Arc<VmFd>,
    region: Arc<Region>,
    doorbell: Arc<EventFd>,
    bases: [Option<u64>; 2],
}

impl Mapping {
    pub(crate) fn new(vm: Arc<VmFd>, region: Arc<Region>, doorbell: Arc<EventFd>) -> Self {
        Self {
            vm,
            region,
            doorbell,
            bases: [None; 2],
        }
    }

    fn memory(&self, base: Option<u64>) -> io::Result<()> {
        for space in [0, 1 << 16] {
            let registration = kvm_bindings::kvm_userspace_memory_region {
                slot: space | 129,
                guest_phys_addr: base.unwrap_or(0),
                memory_size: base.map_or(0, |_| self.region.size()),
                userspace_addr: self.region.host_address(),
                flags: 0,
            };
            // SAFETY: Mapping retains Region until both KVM registrations are removed.
            unsafe { self.vm.set_user_memory_region(registration) }.map_err(io::Error::other)?;
        }
        Ok(())
    }
}

impl Mapper for Mapping {
    fn set_bar(&mut self, bar: u8, base: Option<u64>) -> io::Result<()> {
        let index = if bar == 0 { 0 } else { 1 };
        if self.bases[index] == base {
            return Ok(());
        }
        if bar == 0 {
            if let Some(old) = self.bases[index] {
                self.vm
                    .unregister_ioevent(
                        &self.doorbell,
                        &IoEventAddress::Mmio(old + 12),
                        NoDatamatch,
                    )
                    .map_err(io::Error::other)?;
                self.bases[index] = None;
            }
            if let Some(base) = base {
                self.vm
                    .register_ioevent(
                        &self.doorbell,
                        &IoEventAddress::Mmio(base + 12),
                        NoDatamatch,
                    )
                    .map_err(io::Error::other)?;
            }
        } else {
            if base.is_some_and(|b| {
                b < 0x8000_0000
                    || b % self.region.size() != 0
                    || b.checked_add(self.region.size()).is_none()
            }) {
                return Err(io::Error::other("invalid virtual display BAR2 address"));
            }
            if self.bases[index].is_some() {
                self.memory(None)?;
                self.bases[index] = None;
            }
            if base.is_some()
                && let Err(error) = self.memory(base)
            {
                self.memory(None)?;
                return Err(error);
            }
        }
        self.bases[index] = base;
        Ok(())
    }
}

impl Drop for Mapping {
    fn drop(&mut self) {
        if let Err(error) = self.set_bar(0, None).and_then(|_| self.set_bar(2, None)) {
            log::error!("removing virtual display registrations: {error}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use kvm_ioctls::VcpuExit;
    use vm_memory::{Bytes, GuestAddress, GuestMemoryBackend};
    use vmlab_hv_proto::machine::VirtualDisplay;

    #[test]
    #[cfg_attr(vmlab_ci_no_kvm, ignore = "requires KVM")]
    fn kvm_doorbell_relocation_decode_disable_and_bar2_mapping() {
        if !std::path::Path::new("/dev/kvm").exists() {
            eprintln!("SKIP virtual display KVM registrations: /dev/kvm missing");
            return;
        }
        let kvm = crate::kvm::KvmVm::new().unwrap();
        let region = Region::new(VirtualDisplay::default_memory_bytes()).unwrap();
        let doorbell = Arc::new(EventFd::new(libc::EFD_NONBLOCK).unwrap());
        let mut mapping = Mapping::new(kvm.vm.clone(), region.clone(), doorbell.clone());
        mapping.set_bar(2, Some(1 << 32)).unwrap();
        mapping.set_bar(2, Some(2 << 32)).unwrap();
        assert!(mapping.set_bar(2, Some(0x1000)).is_err());
        mapping.set_bar(2, None).unwrap();

        let memory =
            vmlab_hv_devices::GuestMemory::from_ranges(&[(GuestAddress(0), 0x2000)]).unwrap();
        let registration = kvm_bindings::kvm_userspace_memory_region {
            slot: 0,
            guest_phys_addr: 0,
            memory_size: 0x2000,
            userspace_addr: memory.get_host_address(GuestAddress(0)).unwrap() as u64,
            flags: 0,
        };
        // SAFETY: this test retains memory until after the vCPU is dropped.
        unsafe { kvm.vm.set_user_memory_region(registration) }.unwrap();
        let mut cpu = kvm.vm.create_vcpu(0).unwrap();
        let mut segments = cpu.get_sregs().unwrap();
        segments.cs.base = 0;
        segments.cs.selector = 0;
        segments.ds.limit = u32::MAX;
        segments.ds.g = 1;
        cpu.set_sregs(&segments).unwrap();
        for base in [0x9000_0000u64, 0x9000_1000] {
            mapping.set_bar(0, Some(base)).unwrap();
            let mut code = vec![0x67, 0x66, 0xc7, 0x05];
            code.extend(((base + 12) as u32).to_le_bytes());
            code.extend(0u32.to_le_bytes());
            code.extend([0xe6, 0xf4]);
            memory.write_slice(&code, GuestAddress(0x1000)).unwrap();
            cpu.set_regs(&kvm_bindings::kvm_regs {
                rip: 0x1000,
                rflags: 2,
                ..Default::default()
            })
            .unwrap();
            let exit = cpu.run().unwrap();
            assert!(
                matches!(exit, VcpuExit::IoOut(0xf4, _)),
                "doorbell exit: {exit:?}"
            );
            assert_eq!(doorbell.read().unwrap(), 1);
        }
        mapping.set_bar(0, None).unwrap();
        cpu.set_regs(&kvm_bindings::kvm_regs {
            rip: 0x1000,
            rflags: 2,
            ..Default::default()
        })
        .unwrap();
        assert!(matches!(
            cpu.run().unwrap(),
            VcpuExit::MmioWrite(0x9000_100c, _)
        ));
        assert_eq!(
            doorbell.read().unwrap_err().kind(),
            io::ErrorKind::WouldBlock
        );
    }
}
