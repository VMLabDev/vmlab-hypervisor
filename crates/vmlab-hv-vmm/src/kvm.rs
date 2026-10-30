//! KVM VM setup: the `/dev/kvm` fd, one VM, in-kernel irqchip and PIT, and
//! CPUID passthrough.

use std::sync::Arc;

use kvm_bindings::{CpuId, KVM_MAX_CPUID_ENTRIES, kvm_pit_config};
use kvm_ioctls::{Cap, Kvm, VcpuFd, VmFd};

use crate::error::{VmmError, VmmError as E};

/// Addresses KVM manages inside the PCI hole for real-mode emulation
/// (kernel memory mapped there, not guest RAM).
const KVM_TSS_ADDR: usize = 0xFFFBD000;
const KVM_IDENTITY_MAP_ADDR: u64 = 0xFFFC9000;

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

    /// Create vCPU `id` with the host's supported CPUID passed through.
    pub fn create_vcpu(&self, id: u8) -> Result<VcpuFd, VmmError> {
        let vcpu = self
            .vm
            .create_vcpu(id as u64)
            .map_err(|e| E::ioctl("KVM_CREATE_VCPU", e))?;
        vcpu.set_cpuid2(&self.cpuid)
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
