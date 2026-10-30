//! Hyper-V CPUID policy and KVM enablement, from the Hyper-V TLFS and
//! Linux KVM API sections 4.118 and 6.77.

use std::os::fd::{AsRawFd, RawFd};

use kvm_bindings::{CpuId, KVM_MAX_CPUID_ENTRIES, kvm_cpuid_entry2, kvm_enable_cap};
use kvm_ioctls::{Cap, Kvm, VcpuFd, VmFd};
use vmlab_hv_proto::machine::HyperV;

use crate::error::VmmError;

const BASE: u32 = 0x4000_0000;
const INTERFACE: u32 = 0x3123_7648; // Hv#1

/// The enlightenment set used by vmlab's host-CPU Windows profiles.
/// The caller adds `Avic` only when host APIC virtualization is active.
pub const WINDOWS_FEATURES: &[HyperV] = &[
    HyperV::Relaxed,
    HyperV::Vapic,
    HyperV::Spinlocks,
    HyperV::Vpindex,
    HyperV::Runtime,
    HyperV::Time,
    HyperV::Synic,
    HyperV::Stimer,
    HyperV::Frequencies,
    HyperV::Reset,
    HyperV::Tlbflush,
    HyperV::Ipi,
];

fn invalid(message: impl Into<String>) -> VmmError {
    VmmError::InvalidMachine(message.into())
}

/// Whether the loaded KVM module has hardware APIC virtualization enabled.
pub fn host_apic_virtualization() -> bool {
    [("kvm_intel", "enable_apicv"), ("kvm_amd", "avic")]
        .iter()
        .any(|(module, parameter)| {
            std::fs::read_to_string(format!("/sys/module/{module}/parameters/{parameter}"))
                .is_ok_and(|value| matches!(value.trim(), "Y" | "y" | "1"))
        })
}

/// Apply VM-wide Hyper-V policy after creating the irqchip and PIT.
/// AVIC requires discarding missed PIT ticks, as in vmlab's QEMU path.
pub fn configure_vm(kvm: &Kvm, vm: &VmFd, features: &[HyperV]) -> Result<(), VmmError> {
    if !features.contains(&HyperV::Avic) {
        return Ok(());
    }
    if !host_apic_virtualization() {
        return Err(invalid("Hyper-V AVIC requires host APIC virtualization"));
    }
    if !features.contains(&HyperV::Time) {
        return Err(invalid("Hyper-V AVIC PIT policy requires Hyper-V time"));
    }
    if !kvm.check_extension(Cap::ReinjectControl) {
        return Err(VmmError::KvmCap("PIT reinjection control for Hyper-V AVIC"));
    }
    let control = kvm_bindings::kvm_reinject_control::default();
    // Linux x86 _IO(KVMIO, 0x71), with a pointer despite its _IO encoding.
    // SAFETY: vm is live and control is an initialized UAPI input structure.
    let result = unsafe { libc::ioctl(vm.as_raw_fd(), 0xae71 as libc::c_ulong, &control) };
    if result < 0 {
        return Err(VmmError::Other(format!(
            "KVM_REINJECT_CONTROL: {}",
            std::io::Error::last_os_error()
        )));
    }
    Ok(())
}

/// Query the kernel's Hyper-V capability leaves, not its KVM vendor leaves.
/// kvm-ioctls 0.21 does not wrap this ioctl.
pub fn supported_cpuid(kvm: &Kvm, vcpu: &VcpuFd) -> Result<CpuId, VmmError> {
    let fd = if kvm.check_extension_raw(kvm_bindings::KVM_CAP_SYS_HYPERV_CPUID as _) > 0 {
        kvm.as_raw_fd()
    } else if kvm.check_extension_raw(kvm_bindings::KVM_CAP_HYPERV_CPUID as _) > 0 {
        vcpu.as_raw_fd()
    } else {
        return Err(VmmError::KvmCap("Hyper-V CPUID"));
    };
    query_cpuid(fd)
}

fn query_cpuid(fd: RawFd) -> Result<CpuId, VmmError> {
    let mut cpuid = CpuId::new(KVM_MAX_CPUID_ENTRIES)
        .map_err(|e| invalid(format!("allocating Hyper-V CPUID: {e}")))?;
    // Linux x86 _IOWR(KVMIO, 0xc1, struct kvm_cpuid2). The structure header
    // is eight bytes; its variable array is owned by CpuId.
    const GET_SUPPORTED_HV_CPUID: libc::c_ulong = 0xc008_aec1;
    // SAFETY: fd is a live KVM or vCPU descriptor. CpuId owns a writable
    // kvm_cpuid2 allocation with nent entries, as required by this ioctl.
    let result = unsafe { libc::ioctl(fd, GET_SUPPORTED_HV_CPUID, cpuid.as_mut_fam_struct_ptr()) };
    if result < 0 {
        return Err(VmmError::Other(format!(
            "KVM_GET_SUPPORTED_HV_CPUID: {}",
            std::io::Error::last_os_error()
        )));
    }
    Ok(cpuid)
}

fn leaf(function: u32, eax: u32, ebx: u32, ecx: u32, edx: u32) -> kvm_cpuid_entry2 {
    kvm_cpuid_entry2 {
        function: BASE + function,
        eax,
        ebx,
        ecx,
        edx,
        ..Default::default()
    }
}

/// Replace the overlapping KVM vendor leaves with the requested Hyper-V ABI.
/// Feature bits are checked against KVM before any guest-visible change.
pub fn compose_cpuid(
    base: &CpuId,
    supported: &CpuId,
    features: &[HyperV],
) -> Result<CpuId, VmmError> {
    if features.is_empty() {
        return Ok(base.clone());
    }
    let has = |feature| features.contains(&feature);
    for (feature, dependencies) in [
        (HyperV::Synic, &[HyperV::Vpindex][..]),
        (HyperV::Stimer, &[HyperV::Synic, HyperV::Time][..]),
        (HyperV::Tlbflush, &[HyperV::Vpindex][..]),
        (HyperV::Ipi, &[HyperV::Vpindex][..]),
        (HyperV::Avic, &[HyperV::Synic][..]),
    ] {
        if has(feature) && dependencies.iter().any(|&dep| !has(dep)) {
            return Err(invalid(format!(
                "Hyper-V {feature:?} requires {dependencies:?}"
            )));
        }
    }
    if has(HyperV::Evmbcs) {
        return Err(invalid(
            "Hyper-V enlightened VMCS is not supported by this CPU policy",
        ));
    }
    let mut access = 1 << 5; // Guest OS ID and hypercall page
    let mut privileges = 0;
    let mut misc = 0;
    let mut recommendations = 0;
    if has(HyperV::Runtime) {
        access |= 1;
    }
    if has(HyperV::Time) {
        access |= (1 << 1) | (1 << 9);
    }
    if has(HyperV::Synic) {
        access |= 1 << 2;
        privileges |= 0x30;
    }
    if has(HyperV::Stimer) {
        access |= 1 << 3;
    }
    if has(HyperV::Vapic) {
        access |= 1 << 4;
    }
    if has(HyperV::Vpindex) {
        access |= 1 << 6;
        misc |= 1 << 3;
    }
    if has(HyperV::Reset) {
        access |= 1 << 7;
    }
    if has(HyperV::Frequencies) {
        access |= 1 << 11;
        misc |= 1 << 8;
    }
    if has(HyperV::Reenlightenment) {
        access |= 1 << 13;
    }
    if has(HyperV::Relaxed) {
        recommendations |= 1 << 5;
    }
    if has(HyperV::Tlbflush) {
        recommendations |= (1 << 2) | (1 << 11);
    }
    if has(HyperV::Ipi) {
        recommendations |= (1 << 10) | (1 << 11);
    }
    if has(HyperV::Avic) {
        recommendations |= 1 << 9;
    }
    for (function, requested) in [
        (3, [access, privileges, 0, misc & !(1 << 3)]),
        (4, [recommendations, 0, 0, 0]),
    ] {
        let available = supported
            .as_slice()
            .iter()
            .find(|e| e.function == BASE + function)
            .ok_or(VmmError::KvmCap("Hyper-V feature leaves"))?;
        let caps = [available.eax, available.ebx, available.ecx, available.edx];
        for (want, cap) in requested.into_iter().zip(caps) {
            if want & !cap != 0 {
                return Err(invalid(format!(
                    "KVM lacks Hyper-V leaf {:#x} bits {:#x}",
                    BASE + function,
                    want & !cap
                )));
            }
        }
    }
    let mut entries: Vec<_> = base
        .as_slice()
        .iter()
        .copied()
        .filter(|e| !(BASE..BASE + 0x100).contains(&e.function))
        .collect();
    if let Some(version) = entries.iter_mut().find(|e| e.function == 1) {
        version.ecx |= 1 << 31;
    }
    entries.extend([
        leaf(0, BASE + 5, 0x7263_694d, 0x666f_736f, 0x7648_2074),
        leaf(1, INTERFACE, 0, 0, 0),
        leaf(2, 0x3839, 0x000a_0000, 0, 0),
        leaf(3, access, privileges, 0, misc),
        leaf(
            4,
            recommendations,
            if has(HyperV::Spinlocks) {
                0x1fff
            } else {
                u32::MAX
            },
            0,
            0,
        ),
        leaf(5, u32::MAX, 64, 0, 0),
    ]);
    // Leaves 6..A are reserved for features outside this Windows profile.
    // Return zero rather than leaking host nested-virtualization capabilities.
    entries.extend((6..=10).map(|n| leaf(n, 0, 0, 0, 0)));
    CpuId::from_entries(&entries).map_err(|e| invalid(format!("building Hyper-V CPUID: {e}")))
}

/// Configure a vCPU after topology/VMX/SVM policy and before its first run.
/// The VM must already have an in-kernel irqchip.
pub fn configure_vcpu(kvm: &Kvm, vcpu: &VcpuFd, features: &[HyperV]) -> Result<(), VmmError> {
    if features.is_empty() {
        return Ok(());
    }
    let supported = supported_cpuid(kvm, vcpu)?;
    let base = vcpu
        .get_cpuid2(KVM_MAX_CPUID_ENTRIES)
        .map_err(|e| VmmError::ioctl("KVM_GET_CPUID2", e))?;
    let cpuid = compose_cpuid(&base, &supported, features)?;
    if features.contains(&HyperV::Synic) {
        if !kvm.check_extension(Cap::HypervSynic2) {
            return Err(VmmError::KvmCap("Hyper-V SynIC2"));
        }
        vcpu.enable_cap(&kvm_enable_cap {
            cap: kvm_bindings::KVM_CAP_HYPERV_SYNIC2,
            ..Default::default()
        })
        .map_err(|e| VmmError::ioctl("KVM_ENABLE_CAP(HYPERV_SYNIC2)", e))?;
    }
    vcpu.set_cpuid2(&cpuid)
        .map_err(|e| VmmError::ioctl("KVM_SET_CPUID2(Hyper-V)", e))
}

/// Complete Hyper-V exits that need userspace participation. SynIC notices
/// need no routing changes without VMBus devices. KVM handles timer delivery,
/// TLB flushes and IPIs; unsupported userspace hypercalls return TLFS status 2.
pub(crate) fn handle_exit(run: &mut kvm_bindings::kvm_run) -> Result<(), VmmError> {
    if run.exit_reason != kvm_bindings::KVM_EXIT_HYPERV {
        return Err(invalid("expected a Hyper-V KVM exit"));
    }
    // SAFETY: KVM_EXIT_HYPERV selects the hyperv member of the run union.
    let exit = unsafe { &mut run.__bindgen_anon_1.hyperv };
    match exit.type_ {
        kvm_bindings::KVM_EXIT_HYPERV_SYNIC => Ok(()),
        kvm_bindings::KVM_EXIT_HYPERV_HCALL => {
            exit.u.hcall.result = 2;
            Ok(())
        }
        kind => Err(invalid(format!("unsupported Hyper-V exit {kind}"))),
    }
}

/// KVM's migratable Hyper-V MSRs, including SynIC and synthetic timer state.
/// Read-only counters and write-only reset/EOI registers are not migration state.
pub fn snapshot_msr_indices(kvm: &Kvm, vcpu: &VcpuFd) -> Result<Vec<u32>, VmmError> {
    let cpuid = vcpu
        .get_cpuid2(KVM_MAX_CPUID_ENTRIES)
        .map_err(|e| VmmError::ioctl("KVM_GET_CPUID2", e))?;
    if !cpuid
        .as_slice()
        .iter()
        .any(|e| e.function == BASE + 1 && e.eax == INTERFACE)
    {
        return Ok(Vec::new());
    }
    let indices = kvm
        .get_msr_index_list()
        .map_err(|e| VmmError::ioctl("KVM_GET_MSR_INDEX_LIST", e))?;
    let access = cpuid
        .as_slice()
        .iter()
        .find(|e| e.function == BASE + 3)
        .map_or(0, |e| e.eax);
    let mut state = vec![BASE, BASE + 1];
    for (bit, index) in [(0, 0x10), (6, 2), (9, 0x21), (4, 0x73)] {
        if access & (1 << bit) != 0 {
            state.push(BASE + index);
        }
    }
    // GET_MSR_INDEX_LIST reports the first register of the SynIC and
    // timer groups, not every member. Expand the TLFS register ranges.
    if access & (1 << 2) != 0 {
        state.extend([BASE + 0x80, BASE + 0x82, BASE + 0x83]);
        state.extend((0x90..=0x9f).map(|n| BASE + n));
    }
    if access & (1 << 3) != 0 {
        state.extend((0xb0..=0xb7).map(|n| BASE + n));
    }
    if access & (1 << 13) != 0 {
        state.extend([BASE + 0x106, BASE + 0x107, BASE + 0x108]);
    }
    for index in &state {
        let group = match *index - BASE {
            0x82..=0x9f => BASE + 0x80,
            0xb0..=0xb7 => BASE + 0xb0,
            _ => *index,
        };
        if !indices.as_slice().contains(&group) {
            return Err(invalid(format!(
                "KVM cannot migrate Hyper-V MSR {index:#x}"
            )));
        }
    }
    Ok(state)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::kvm::KvmVm;
    use kvm_bindings::{Msrs, kvm_msr_entry};
    use kvm_ioctls::VcpuExit;
    use vm_memory::{Bytes, GuestAddress};

    fn full_support() -> CpuId {
        CpuId::from_entries(&[
            leaf(3, u32::MAX, u32::MAX, 0, u32::MAX),
            leaf(4, u32::MAX, 0, 0, 0),
        ])
        .unwrap()
    }

    #[test]
    fn composition_matches_windows_capture() {
        let mut features = WINDOWS_FEATURES.to_vec();
        features.push(HyperV::Avic);
        let result = compose_cpuid(&CpuId::new(0).unwrap(), &full_support(), &features).unwrap();
        let capture = include_str!("../../../docs/hwref/q35-ovmf-win11/guest/cpuid.txt");
        for entry in result.as_slice().iter().filter(|e| e.function <= BASE + 5) {
            let expected = format!(
                "{:#010x} 0x00: eax={:#010x} ebx={:#010x} ecx={:#010x} edx={:#010x}",
                entry.function, entry.eax, entry.ebx, entry.ecx, entry.edx
            );
            assert!(capture.lines().any(|line| line == expected), "{expected}");
        }
        assert!(
            result
                .as_slice()
                .iter()
                .filter(|e| e.function > BASE + 5)
                .all(|e| (e.eax, e.ebx, e.ecx, e.edx) == (0, 0, 0, 0))
        );
    }

    #[test]
    fn dependencies_and_unsupported_bits_fail_before_installation() {
        let base = CpuId::new(0).unwrap();
        assert!(compose_cpuid(&base, &full_support(), &[HyperV::Stimer]).is_err());
        assert!(compose_cpuid(&base, &full_support(), &[HyperV::Ipi]).is_err());
        let missing = CpuId::from_entries(&[leaf(3, 0, 0, 0, 0), leaf(4, 0, 0, 0, 0)]).unwrap();
        assert!(compose_cpuid(&base, &missing, WINDOWS_FEATURES).is_err());
        assert_eq!(
            compose_cpuid(&base, &missing, &[]).unwrap().as_slice(),
            base.as_slice()
        );
    }

    #[test]
    fn preserves_architectural_policy_and_removes_kvm_vendor_collision() {
        let base = CpuId::from_entries(&[
            kvm_cpuid_entry2 {
                function: 1,
                ecx: 0x1234,
                ..Default::default()
            },
            leaf(0, BASE + 1, 0x4b4d564b, 0, 0),
            leaf(1, 0xffff, 0, 0, 0),
        ])
        .unwrap();
        let cpuid = compose_cpuid(&base, &full_support(), WINDOWS_FEATURES).unwrap();
        assert_eq!(cpuid.as_slice()[0].ecx, 0x8000_1234);
        assert_eq!(
            cpuid
                .as_slice()
                .iter()
                .filter(|e| e.function == BASE)
                .count(),
            1
        );
        assert_eq!(
            cpuid
                .as_slice()
                .iter()
                .find(|e| e.function == BASE + 4)
                .unwrap()
                .eax,
            0xc24
        );
    }

    fn kvm() -> Option<KvmVm> {
        if !std::path::Path::new("/dev/kvm").exists() {
            eprintln!("skipping Hyper-V guest: /dev/kvm is unavailable");
            return None;
        }
        Some(KvmVm::new().expect("create Hyper-V test VM"))
    }

    fn msr(index: u32, data: u64) -> kvm_msr_entry {
        kvm_msr_entry {
            index,
            data,
            ..Default::default()
        }
    }

    #[test]
    fn avic_policy_requires_host_support_and_discards_missed_pit_ticks() {
        let Some(vm) = kvm() else { return };
        let mut features = WINDOWS_FEATURES.to_vec();
        features.push(HyperV::Avic);
        let result = configure_vm(&vm.kvm, &vm.vm, &features);
        if !host_apic_virtualization() {
            assert!(result.is_err());
            eprintln!("skipping AVIC enablement: host APIC virtualization is disabled");
            return;
        }
        result.unwrap();
        let cpu = vm.create_vcpu(0).unwrap();
        configure_vcpu(&vm.kvm, &cpu, &features).unwrap();
        let cpuid = cpu.get_cpuid2(KVM_MAX_CPUID_ENTRIES).unwrap();
        let recommendations = cpuid
            .as_slice()
            .iter()
            .find(|e| e.function == BASE + 4)
            .unwrap();
        assert_ne!(recommendations.eax & (1 << 9), 0);
    }

    #[test]
    fn tiny_guest_reads_cpuid_and_hyperv_msrs() {
        let Some(vm) = kvm() else { return };
        let ram =
            crate::memory::GuestRam::anonymous(crate::layout::MemLayout::new(16).unwrap()).unwrap();
        crate::memory::register_memslots(&vm.vm, &ram).unwrap();
        let mut vcpu = vm
            .create_vcpu_with_policy(0, 1, vmlab_hv_proto::machine::CpuPolicy::Host)
            .unwrap();
        configure_vcpu(&vm.kvm, &vcpu, WINDOWS_FEATURES).unwrap();
        // Flat real mode, operand-size prefixes select 32-bit registers.
        // Store each CPUID result and each RDMSR pair in guest RAM.
        let mut code = Vec::new();
        let mut address = 0x9000u16;
        let mut store = |register: u8, code: &mut Vec<u8>| {
            code.extend([0x66, 0x89, 0x06 | (register << 3)]);
            code.extend(address.to_le_bytes());
            address += 4;
        };
        for function in BASE..=BASE + 10 {
            code.extend([0x66, 0xb8]);
            code.extend(function.to_le_bytes());
            code.extend([0x66, 0x31, 0xc9, 0x0f, 0xa2]);
            for register in [0, 3, 1, 2] {
                store(register, &mut code);
            }
        }
        for index in [BASE, BASE + 2, BASE + 0x20] {
            code.extend([0x66, 0xb9]);
            code.extend(index.to_le_bytes());
            code.extend([0x0f, 0x32]);
            store(0, &mut code);
            store(2, &mut code);
        }
        code.extend([0xe6, 0xe9]); // Completion OUT, avoiding HLT with in-kernel irqchip.
        ram.mem.write_slice(&code, GuestAddress(0x8000)).unwrap();
        let mut sregs = vcpu.get_sregs().unwrap();
        sregs.cs.base = 0;
        sregs.cs.selector = 0;
        vcpu.set_sregs(&sregs).unwrap();
        let mut regs = vcpu.get_regs().unwrap();
        regs.rip = 0x8000;
        regs.rflags = 2;
        vcpu.set_regs(&regs).unwrap();
        assert!(matches!(vcpu.run().unwrap(), VcpuExit::IoOut(0xe9, _)));
        let cpuid = vcpu.get_cpuid2(KVM_MAX_CPUID_ENTRIES).unwrap();
        let mut address = 0x9000;
        for function in BASE..=BASE + 10 {
            let entry = cpuid
                .as_slice()
                .iter()
                .find(|e| e.function == function)
                .unwrap();
            for expected in [entry.eax, entry.ebx, entry.ecx, entry.edx] {
                let mut bytes = [0; 4];
                ram.mem
                    .read_slice(&mut bytes, GuestAddress(address))
                    .unwrap();
                assert_eq!(u32::from_le_bytes(bytes), expected, "CPUID {function:#x}");
                address += 4;
            }
        }
        for expected in [Some(0), Some(0), None] {
            let mut bytes = [0; 8];
            ram.mem
                .read_slice(&mut bytes, GuestAddress(address))
                .unwrap();
            let value = u64::from_le_bytes(bytes);
            if let Some(expected) = expected {
                assert_eq!(value, expected);
            } else {
                assert!(value > 0);
            }
            address += 8;
        }
    }

    #[test]
    fn hyperv_migration_msrs_round_trip_to_fresh_vcpu() {
        let Some(source) = kvm() else { return };
        let destination = KvmVm::new().unwrap();
        let src = source.create_vcpu(0).unwrap();
        let dst = destination.create_vcpu(0).unwrap();
        assert!(snapshot_msr_indices(&source.kvm, &src).unwrap().is_empty());
        for (vm, fd) in [(&source, &src), (&destination, &dst)] {
            configure_vcpu(&vm.kvm, fd, WINDOWS_FEATURES).unwrap();
        }
        let state = Msrs::from_entries(&[
            msr(BASE, 0x8100_0000_0000_0001),
            msr(BASE + 0x10, 123456),
            msr(BASE + 0x80, 1),
            msr(BASE + 0x90, (1 << 16) | 0x40),
            msr(BASE + 0xb1, 987654),
        ])
        .unwrap();
        assert_eq!(src.set_msrs(&state).unwrap(), state.as_slice().len());
        let indices = snapshot_msr_indices(&source.kvm, &src).unwrap();
        for e in state.as_slice() {
            assert!(indices.contains(&e.index), "missing {:#x}", e.index);
        }
        let mut saved =
            Msrs::from_entries(&indices.iter().map(|&i| msr(i, 0)).collect::<Vec<_>>()).unwrap();
        assert_eq!(src.get_msrs(&mut saved).unwrap(), indices.len());
        let bytes = serde_json::to_vec(saved.as_slice()).unwrap();
        let decoded: Vec<kvm_msr_entry> = serde_json::from_slice(&bytes).unwrap();
        let saved = Msrs::from_entries(&decoded).unwrap();
        assert_eq!(dst.set_msrs(&saved).unwrap(), indices.len());
        let mut restored = Msrs::from_entries(state.as_slice()).unwrap();
        assert_eq!(dst.get_msrs(&mut restored).unwrap(), state.as_slice().len());
        for (actual, initial) in restored.as_slice().iter().zip(state.as_slice()) {
            if actual.index == BASE + 0x10 {
                let saved_runtime = saved
                    .as_slice()
                    .iter()
                    .find(|e| e.index == actual.index)
                    .unwrap()
                    .data;
                // VP_RUNTIME counts execution time in 100 ns units, including
                // time spent in KVM ioctls. It can advance while being read.
                assert!(
                    actual.data.wrapping_sub(saved_runtime) < 10_000_000,
                    "runtime was not restored: saved={saved_runtime}, actual={}",
                    actual.data
                );
            } else {
                assert_eq!(actual, initial);
            }
        }
    }

    #[test]
    fn control_socket_snapshot_restores_hyperv_state_and_guest_reset() {
        use crate::snapshot::{DiskNode, SnapshotRegistry};
        use crate::vm::{ExitReason, Vm, VmShared};
        use crate::{
            bus::{IoBus, MmioBus},
            pci::PciRoot,
            pm::{PmDevice, SleepSignal},
        };
        use parking_lot::Mutex;
        use std::sync::Arc;
        use vmlab_hv_proto::{
            client::Client,
            control::{Command, Outcome, ShutdownReason},
        };

        struct NullIrq;
        impl vmlab_hv_devices::IrqLine for NullIrq {
            fn set_level(&self, _: bool) {}
        }
        let Some(kvm) = kvm() else { return };
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.qcow2");
        vmlab_disk::ops::create_qcow2(&path, 16 << 20, None, None).unwrap();
        let layout = crate::layout::MemLayout::new(16).unwrap();
        let ram = crate::memory::GuestRam::anonymous(layout.clone()).unwrap();
        crate::memory::register_memslots(&kvm.vm, &ram).unwrap();
        // Read the restored guest OS ID, leave it in RAM, enable SynIC,
        // then request reset through the Hyper-V reset MSR.
        let code = [
            0x66, 0xb9, 0, 0, 0, 0x40, 0x0f, 0x32, 0x66, 0xa3, 0, 0x90, 0x66, 0xb9, 0x80, 0, 0,
            0x40, 0x66, 0xb8, 1, 0, 0, 0, 0x66, 0x31, 0xd2, 0x0f, 0x30, 0x66, 0xb9, 3, 0, 0, 0x40,
            0x0f, 0x30,
        ];
        ram.mem.write_slice(&code, GuestAddress(0x8000)).unwrap();
        let mut cpu = kvm.create_vcpu(0).unwrap();
        configure_vcpu(&kvm.kvm, &cpu, WINDOWS_FEATURES).unwrap();
        crate::loader::init_real_mode(&mut cpu, 0x8000).unwrap();
        let saved_state = Msrs::from_entries(&[
            msr(BASE, 42),
            msr(BASE + 0x90, 0x10040),
            msr(BASE + 0xb1, 789),
        ])
        .unwrap();
        assert_eq!(cpu.set_msrs(&saved_state).unwrap(), 3);
        let signal = Arc::new(SleepSignal::default());
        let pm = Arc::new(Mutex::new(PmDevice::new(Arc::new(NullIrq), signal.sink())));
        let shared = VmShared::new(
            kvm,
            IoBus::new(),
            MmioBus::new(),
            Arc::new(Mutex::new(PciRoot::new(layout.mmio64_base))),
            pm,
            signal,
            layout.mmio64_base,
        );
        shared.set_snapshot_registry(Arc::new(SnapshotRegistry {
            digest: [7; 32],
            nodes: vec![],
            disks: vec![DiskNode {
                id: "scratch".into(),
                writable: true,
                vmstate: true,
                image: Arc::new(Mutex::new(
                    vmlab_disk::open(&path, vmlab_disk::Format::Qcow2, false).unwrap(),
                )),
            }],
        }));
        let vm = Vm::from_parts(shared, vec![(0, cpu)], ram).unwrap();
        let handle = vm.handle();
        crate::control::serve(&dir.path().join("control.sock"), &handle).unwrap();
        let mut client = Client::connect(dir.path().join("control.sock")).unwrap();
        let outcome = client
            .call(Command::SnapshotSave {
                name: "hyperv".into(),
            })
            .unwrap();
        assert!(matches!(outcome, Outcome::Ok { .. }), "{outcome:?}");
        {
            let cpus = handle.shared().vcpus.lock();
            let fd = cpus[0].1.lock();
            let changed = Msrs::from_entries(&[msr(BASE, 99), msr(BASE + 0xb1, 1000)]).unwrap();
            assert_eq!(fd.set_msrs(&changed).unwrap(), 2);
        }
        let outcome = client
            .call(Command::SnapshotLoad {
                name: "hyperv".into(),
            })
            .unwrap();
        assert!(matches!(outcome, Outcome::Ok { .. }), "{outcome:?}");
        {
            let cpus = handle.shared().vcpus.lock();
            let fd = cpus[0].1.lock();
            let mut restored = Msrs::from_entries(saved_state.as_slice()).unwrap();
            assert_eq!(fd.get_msrs(&mut restored).unwrap(), 3);
            assert_eq!(restored.as_slice(), saved_state.as_slice());
        }
        let outcome = client.call(Command::Resume).unwrap();
        assert!(matches!(outcome, Outcome::Ok { .. }), "{outcome:?}");
        // A bounded watchdog also releases the VM if the payload faults.
        let (done_tx, done_rx) = std::sync::mpsc::channel();
        let watchdog_handle = handle.clone();
        let watchdog = std::thread::spawn(move || {
            if done_rx
                .recv_timeout(std::time::Duration::from_secs(5))
                .is_err()
            {
                watchdog_handle.quit();
            }
        });
        assert_eq!(vm.wait(), ExitReason::Shutdown(ShutdownReason::GuestReset));
        done_tx.send(()).unwrap();
        watchdog.join().unwrap();
        let mut value = [0; 4];
        handle
            .shared()
            .ram()
            .unwrap()
            .mem
            .read_slice(&mut value, GuestAddress(0x9000))
            .unwrap();
        assert_eq!(u32::from_le_bytes(value), 42);
    }
}
