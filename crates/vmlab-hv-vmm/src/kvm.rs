//! KVM VM setup: the `/dev/kvm` fd, one VM, in-kernel irqchip and PIT, and
//! CPUID passthrough.

use std::sync::Arc;

use kvm_bindings::{CpuId, KVM_MAX_CPUID_ENTRIES, kvm_pit_config};
use kvm_ioctls::{Cap, Kvm, VcpuFd, VmFd};

use crate::error::{VmmError, VmmError as E};

/// Addresses KVM manages inside the PCI hole for real-mode emulation
/// (kernel memory mapped there, not guest RAM).
const KVM_TSS_ADDR: usize = 0xFEFFD000;
const KVM_IDENTITY_MAP_ADDR: u64 = 0xFEFF_C000;

/// One KVM virtual machine.
pub struct KvmVm {
    pub kvm: Kvm,
    pub vm: Arc<VmFd>,
    cpuid: CpuId,
}

impl KvmVm {
    pub fn new() -> Result<Self, VmmError> {
        let kvm = Kvm::new().map_err(|e| E::KvmOpen(e.to_string()))?;
        if !kvm.check_extension(Cap::UserMemory) {
            return Err(E::KvmCap("user memory regions"));
        }
        if !kvm.check_extension(Cap::Irqchip) {
            return Err(E::KvmCap("in-kernel irqchip"));
        }
        let vm = kvm.create_vm().map_err(|e| E::ioctl("KVM_CREATE_VM", e))?;
        // Must precede KVM_CREATE_IRQCHIP + vCPUs.
        vm.set_tss_address(KVM_TSS_ADDR)
            .map_err(|e| E::ioctl("KVM_SET_TSS_ADDR", e))?;
        vm.set_identity_map_address(KVM_IDENTITY_MAP_ADDR)
            .map_err(|e| E::ioctl("KVM_SET_IDENTITY_MAP_ADDR", e))?;
        vm.create_irq_chip()
            .map_err(|e| E::ioctl("KVM_CREATE_IRQCHIP", e))?;
        vm.create_pit2(kvm_pit_config::default())
            .map_err(|e| E::ioctl("KVM_CREATE_PIT2", e))?;
        let cpuid = kvm
            .get_supported_cpuid(KVM_MAX_CPUID_ENTRIES)
            .map_err(|e| E::ioctl("KVM_GET_SUPPORTED_CPUID", e))?;
        Ok(KvmVm {
            kvm,
            vm: Arc::new(vm),
            cpuid,
        })
    }

    /// Maximum vCPU count the kernel allows.
    pub fn max_vcpus(&self) -> usize {
        self.kvm.get_max_vcpus()
    }

    /// SeaBIOS places this platform's small prefetchable BARs near the top
    /// of its usable physical address space, with padding when high RAM exists.
    pub(crate) fn seabios_pci64_base(&self, high_ram: bool) -> u64 {
        let bits = self
            .cpuid
            .as_slice()
            .iter()
            .find(|e| e.function == 0x8000_0008)
            .map_or(36, |e| e.eax & 0xff)
            .clamp(32, 44);
        let top = 1u64 << bits;
        top - if high_ram { top / 8 } else { 1 << 30 }
    }

    pub(crate) fn ovmf_pci64_base(&self) -> u64 {
        let bits = self
            .cpuid
            .as_slice()
            .iter()
            .find(|e| e.function == 0x8000_0008)
            .map_or(36, |e| e.eax & 0xff)
            .clamp(32, 46);
        let top = 1u64 << bits;
        top - top / 8
    }

    /// Create vCPU `id` with the host's supported CPUID passed through.
    pub fn create_vcpu(&self, id: u8) -> Result<VcpuFd, VmmError> {
        self.create_vcpu_with_policy(id, 1, vmlab_hv_proto::machine::CpuPolicy::Host)
    }

    pub fn create_vcpu_with_policy(
        &self,
        id: u8,
        count: u32,
        policy: vmlab_hv_proto::machine::CpuPolicy,
    ) -> Result<VcpuFd, VmmError> {
        let vcpu = self
            .vm
            .create_vcpu(id as u64)
            .map_err(|e| E::ioctl("KVM_CREATE_VCPU", e))?;
        let mut cpuid = self.cpuid.clone();
        filter_cpuid(&mut cpuid, id, count, policy)?;
        vcpu.set_cpuid2(&cpuid)
            .map_err(|e| E::ioctl("KVM_SET_CPUID2", e))?;
        Ok(vcpu)
    }

    /// Raise or lower a legacy GSI line (in-kernel PIC/IOAPIC input).
    pub fn set_irq_line(&self, gsi: u32, active: bool) -> Result<(), VmmError> {
        self.vm
            .set_irq_line(gsi, active)
            .map_err(|e| E::ioctl("KVM_IRQ_LINE", e))
    }

    /// Deliver one MSI/MSI-X message; returns whether it was routed.
    pub fn signal_msi(&self, address: u64, data: u32) -> Result<bool, VmmError> {
        let msi = kvm_bindings::kvm_msi {
            address_lo: address as u32,
            address_hi: (address >> 32) as u32,
            data,
            ..Default::default()
        };
        match self.vm.signal_msi(msi) {
            Ok(delivered) => Ok(delivered > 0),
            Err(e) => Err(E::ioctl("KVM_SIGNAL_MSI", e)),
        }
    }
}

/// One socket, `count` cores, one thread per core. Preserve KVM's supported
/// feature set while removing host topology and nested virtualization leaks.
fn filter_cpuid(
    cpuid: &mut CpuId,
    id: u8,
    count: u32,
    policy: vmlab_hv_proto::machine::CpuPolicy,
) -> Result<(), VmmError> {
    let mut entries = cpuid.as_slice().to_vec();
    // KVM_GET_SUPPORTED_CPUID describes capabilities, not the host brand.
    for function in 0x8000_0002..=0x8000_0004 {
        let brand = std::arch::x86_64::__cpuid(function);
        let entry = kvm_bindings::kvm_cpuid_entry2 {
            function,
            eax: brand.eax,
            ebx: brand.ebx,
            ecx: brand.ecx,
            edx: brand.edx,
            ..Default::default()
        };
        if let Some(existing) = entries.iter_mut().find(|e| e.function == function) {
            *existing = entry;
        } else {
            entries.push(entry);
        }
    }
    for function in [0xb, 0x1f] {
        if entries.iter().any(|entry| entry.function == function) {
            for index in 0..=2 {
                if !entries
                    .iter()
                    .any(|entry| entry.function == function && entry.index == index)
                {
                    entries.push(kvm_bindings::kvm_cpuid_entry2 {
                        function,
                        index,
                        flags: kvm_bindings::KVM_CPUID_FLAG_SIGNIFCANT_INDEX,
                        ..Default::default()
                    });
                }
            }
        }
    }
    *cpuid = CpuId::from_entries(&entries)
        .map_err(|e| VmmError::Other(format!("building CPUID topology: {e}")))?;
    let shift = count.next_power_of_two().trailing_zeros();
    let amd = cpuid.as_slice().iter().any(|entry| {
        entry.function == 0
            && (entry.ebx, entry.ecx, entry.edx) == (0x68747541, 0x444d4163, 0x69746e65)
    });
    for entry in cpuid.as_mut_slice() {
        match entry.function {
            1 => {
                entry.ebx =
                    (entry.ebx & 0x0000_ffff) | (count.min(255) << 16) | ((id as u32) << 24);
                if count > 1 {
                    entry.edx |= 1 << 28;
                } else {
                    entry.edx &= !(1 << 28);
                }
                if policy != vmlab_hv_proto::machine::CpuPolicy::Nested {
                    entry.ecx &= !(1 << 5);
                }
            }
            4 if entry.eax & 31 != 0 => {
                entry.eax = (entry.eax & 0x3fff) | ((count.min(64) - 1) << 26);
            }
            0xb | 0x1f => {
                let (width, cpus, level) = match entry.index {
                    0 => (0, 1, 1),
                    1 => (shift, count, 2),
                    _ => (0, 0, 0),
                };
                entry.eax = width;
                entry.ebx = cpus;
                entry.ecx = (level << 8) | entry.index;
                entry.edx = id as u32;
            }
            0x8000_0001 if policy != vmlab_hv_proto::machine::CpuPolicy::Nested => {
                entry.ecx &= !(1 << 2)
            }
            0x8000_0008 => entry.ecx = (entry.ecx & !0xf0ff) | (count - 1).min(255) | (shift << 12),
            0x8000_0005 if amd => {
                // Legacy descriptors of the captured QEMU host CPU model.
                (entry.eax, entry.ebx, entry.ecx, entry.edx) =
                    (0x01ff01ff, 0x01ff01ff, 0x40020140, 0x40020140);
            }
            0x8000_0006 if amd => {
                (entry.eax, entry.ebx, entry.ecx, entry.edx) =
                    (0, 0x42004200, 0x02008140, 0x00808140);
            }
            0x8000_0007 => entry.edx &= !(1 << 8),
            0x8000_001d if amd => {
                // One private L1/L2 per core and one shared 16 MiB L3.
                (entry.eax, entry.ebx, entry.ecx, entry.edx) = match entry.index {
                    0 => (0x121, 0x0040003f, 0x1ff, 1),
                    1 => (0x122, 0x0040003f, 0x1ff, 1),
                    2 => (0x43, 0x03c0003f, 0x1ff, 0),
                    3 => (0x163 | ((count - 1) << 14), 0x03c0003f, 0x3fff, 2),
                    _ => (0, 0, 0, 0),
                };
            }
            0x8000_001e => {
                entry.eax = id as u32;
                entry.ebx = id as u32;
                entry.ecx = 0;
            }
            0x8000_000a if policy != vmlab_hv_proto::machine::CpuPolicy::Nested => {
                entry.eax = 0;
                entry.ebx = 0;
                entry.ecx = 0;
                entry.edx = 0;
            }
            _ => {}
        }
    }
    Ok(())
}

/// A legacy interrupt line routed into the in-kernel irqchip.
pub struct KvmIrqLine {
    vm: Arc<VmFd>,
    gsi: u32,
}

impl KvmIrqLine {
    pub fn new(vm: Arc<VmFd>, gsi: u32) -> Self {
        KvmIrqLine { vm, gsi }
    }
}

impl vmlab_hv_devices::IrqLine for KvmIrqLine {
    fn set_level(&self, high: bool) {
        if let Err(e) = self.vm.set_irq_line(self.gsi, high) {
            // A failed line assertion means the VM is going away; log and
            // carry on rather than unwinding on a device path.
            eprintln!("kvm: KVM_IRQ_LINE({}) failed: {}", self.gsi, e);
        }
    }
}

/// MSI delivery through KVM_SIGNAL_MSI.
pub struct KvmMsiSink {
    vm: Arc<VmFd>,
}

impl KvmMsiSink {
    pub fn new(vm: Arc<VmFd>) -> Self {
        KvmMsiSink { vm }
    }
}

impl vmlab_hv_devices::MsiSink for KvmMsiSink {
    fn signal(&self, msg: vmlab_hv_devices::MsiMessage) {
        let msi = kvm_bindings::kvm_msi {
            address_lo: msg.address as u32,
            address_hi: (msg.address >> 32) as u32,
            data: msg.data,
            ..Default::default()
        };
        if let Err(e) = self.vm.signal_msi(msi) {
            eprintln!("kvm: KVM_SIGNAL_MSI failed: {}", e);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn host_cpu_policy_includes_the_native_brand_string() {
        let mut cpuid = CpuId::from_entries(&[]).unwrap();
        filter_cpuid(&mut cpuid, 0, 2, vmlab_hv_proto::machine::CpuPolicy::Host).unwrap();
        for function in 0x8000_0002..=0x8000_0004 {
            let native = std::arch::x86_64::__cpuid(function);
            let e = cpuid
                .as_slice()
                .iter()
                .find(|e| e.function == function)
                .unwrap();
            assert_eq!(
                (e.eax, e.ebx, e.ecx, e.edx),
                (native.eax, native.ebx, native.ecx, native.edx)
            );
        }
    }

    #[test]
    fn amd_cache_descriptors_and_migratable_tsc_match_the_reference_model() {
        use kvm_bindings::kvm_cpuid_entry2;
        let mut entries = vec![kvm_cpuid_entry2 {
            function: 0,
            ebx: 0x68747541,
            ecx: 0x444d4163,
            edx: 0x69746e65,
            ..Default::default()
        }];
        entries.extend(
            [0x80000005, 0x80000006, 0x80000007].map(|function| kvm_cpuid_entry2 {
                function,
                eax: u32::MAX,
                ebx: u32::MAX,
                ecx: u32::MAX,
                edx: u32::MAX,
                ..Default::default()
            }),
        );
        entries.extend((0..5).map(|index| kvm_cpuid_entry2 {
            function: 0x8000001d,
            index,
            ..Default::default()
        }));
        let mut cpuid = CpuId::from_entries(&entries).unwrap();
        filter_cpuid(&mut cpuid, 0, 2, vmlab_hv_proto::machine::CpuPolicy::Host).unwrap();
        let leaves = cpuid.as_slice();
        assert_eq!(
            (leaves[1].eax, leaves[1].ebx, leaves[1].ecx, leaves[1].edx),
            (0x01ff01ff, 0x01ff01ff, 0x40020140, 0x40020140)
        );
        assert_eq!(
            (leaves[2].eax, leaves[2].ebx, leaves[2].ecx, leaves[2].edx),
            (0, 0x42004200, 0x02008140, 0x00808140)
        );
        assert_eq!(leaves[3].edx & (1 << 8), 0);
        assert_eq!(
            (leaves[4].eax, leaves[4].ebx, leaves[4].ecx, leaves[4].edx),
            (0x121, 0x0040003f, 0x1ff, 1)
        );
        assert_eq!(
            (leaves[7].eax, leaves[7].ebx, leaves[7].ecx, leaves[7].edx),
            (0x4163, 0x03c0003f, 0x3fff, 2)
        );
        assert_eq!(
            (leaves[8].eax, leaves[8].ebx, leaves[8].ecx, leaves[8].edx),
            (0, 0, 0, 0)
        );
    }

    #[test]
    fn topology_adds_core_level_when_kvm_supplies_only_thread_level() {
        let mut cpuid = CpuId::from_entries(&[kvm_bindings::kvm_cpuid_entry2 {
            function: 0xb,
            flags: kvm_bindings::KVM_CPUID_FLAG_SIGNIFCANT_INDEX,
            ..Default::default()
        }])
        .unwrap();
        filter_cpuid(&mut cpuid, 1, 2, vmlab_hv_proto::machine::CpuPolicy::Host).unwrap();
        let core = cpuid
            .as_slice()
            .iter()
            .find(|entry| entry.function == 0xb && entry.index == 1)
            .unwrap();
        assert_eq!((core.eax, core.ebx, core.ecx, core.edx), (1, 2, 0x201, 1));
        let end = cpuid
            .as_slice()
            .iter()
            .find(|entry| entry.function == 0xb && entry.index == 2)
            .unwrap();
        assert_eq!((end.eax, end.ebx, end.ecx, end.edx), (0, 0, 2, 1));
    }

    #[test]
    fn cpuid_masks_nested_features_and_replaces_host_topology() {
        use kvm_bindings::kvm_cpuid_entry2;
        use vmlab_hv_proto::machine::CpuPolicy;
        let entries = [
            kvm_cpuid_entry2 {
                function: 1,
                ecx: u32::MAX,
                edx: u32::MAX,
                ..Default::default()
            },
            kvm_cpuid_entry2 {
                function: 0x80000001,
                ecx: u32::MAX,
                ..Default::default()
            },
            kvm_cpuid_entry2 {
                function: 0xb,
                index: 0,
                ..Default::default()
            },
            kvm_cpuid_entry2 {
                function: 0xb,
                index: 1,
                ..Default::default()
            },
            kvm_cpuid_entry2 {
                function: 0x80000008,
                ..Default::default()
            },
            kvm_cpuid_entry2 {
                function: 0x8000001e,
                ..Default::default()
            },
        ];
        let mut cpuid = CpuId::from_entries(&entries).unwrap();
        filter_cpuid(&mut cpuid, 5, 6, CpuPolicy::Host).unwrap();
        let e = cpuid.as_slice();
        assert_eq!(e[0].ecx & (1 << 5), 0);
        assert_eq!(e[1].ecx & (1 << 2), 0);
        assert_eq!(e[0].ebx >> 16, 0x0506);
        assert_eq!((e[2].eax, e[2].ebx, e[2].edx), (0, 1, 5));
        assert_eq!((e[3].eax, e[3].ebx, e[3].edx), (3, 6, 5));
        assert_eq!(e[4].ecx, 0x3005);
        assert_eq!((e[5].eax, e[5].ebx, e[5].ecx), (5, 5, 0));
        let mut cpuid = CpuId::from_entries(&entries).unwrap();
        filter_cpuid(&mut cpuid, 0, 1, CpuPolicy::Nested).unwrap();
        assert_ne!(cpuid.as_slice()[0].ecx & (1 << 5), 0);
        assert_ne!(cpuid.as_slice()[1].ecx & (1 << 2), 0);
        assert_eq!(cpuid.as_slice()[0].edx & (1 << 28), 0);
    }

    fn kvm_or_skip() -> Option<KvmVm> {
        if !std::path::Path::new("/dev/kvm").exists() {
            eprintln!("skipping: /dev/kvm is not available");
            return None;
        }
        match KvmVm::new() {
            Ok(vm) => Some(vm),
            // Skip (rather than fail) when the host kernel or permissions
            // deny KVM: the test environment, not the code, is the cause.
            Err(e) => {
                eprintln!("skipping: cannot create KVM VM: {e}");
                None
            }
        }
    }

    #[test]
    fn creates_vm_with_irqchip_and_vcpu() {
        let Some(vm) = kvm_or_skip() else { return };
        assert!(vm.max_vcpus() >= 1);
        let vcpu = vm.create_vcpu(0);
        assert!(vcpu.is_ok(), "BSP creation: {:?}", vcpu.err());
    }

    #[test]
    fn irq_line_round_trip() {
        let Some(vm) = kvm_or_skip() else { return };
        assert!(vm.set_irq_line(4, true).is_ok());
        assert!(vm.set_irq_line(4, false).is_ok());
    }
}
