//! Host facts from CPUID and an unstarted, memoryless KVM vCPU.

use kvm_ioctls::Kvm;
use vmlab_hv_proto::host::HostFacts;

use crate::VmmError;

pub fn probe() -> Result<HostFacts, VmmError> {
    let invariant_tsc = std::arch::x86_64::__cpuid(0x8000_0000).eax >= 0x8000_0007
        && std::arch::x86_64::__cpuid(0x8000_0007).edx & (1 << 8) != 0;
    let mut facts = HostFacts {
        kvm: false,
        tsc_khz: None,
        invariant_tsc,
        stable_tsc: false,
    };
    let kvm = match Kvm::new() {
        Ok(kvm) => kvm,
        Err(error)
            if matches!(
                error.errno(),
                libc::ENOENT | libc::ENODEV | libc::EACCES | libc::EPERM
            ) =>
        {
            return Ok(facts);
        }
        Err(error) => return Err(VmmError::KvmOpen(error.to_string())),
    };
    let vm = kvm
        .create_vm()
        .map_err(|e| VmmError::ioctl("KVM_CREATE_VM (host probe)", e))?;
    let cpu = vm
        .create_vcpu(0)
        .map_err(|e| VmmError::ioctl("KVM_CREATE_VCPU (host probe)", e))?;
    facts.kvm = true;
    match cpu.get_tsc_khz() {
        Ok(khz) if khz > 0 => {
            facts.tsc_khz = Some(khz);
            facts.stable_tsc = true;
        }
        Ok(_) => {}
        // The KVM API reports EIO when the host TSC is unstable.
        Err(error) if error.errno() == libc::EIO => {}
        Err(error) => return Err(VmmError::ioctl("KVM_GET_TSC_KHZ (host probe)", error)),
    }
    Ok(facts)
}
