//! Named Intel CPU and per-machine userspace handling of unknown MSRs.

use crate::{error::VmmError, kvm::KvmVm};
use kvm_bindings::{CpuId, KVM_MAX_CPUID_ENTRIES, kvm_cpuid_entry2, kvm_enable_cap};
use kvm_ioctls::{Cap, MsrExitReason, VcpuFd};
use serde::Deserialize;
use vmlab_hv_proto::machine::{MachineDescription, MsrPolicy};

const APIC_BUS_KHZ: u32 = 1_000_000;

#[derive(Deserialize)]
struct Capture {
    leaves: Vec<Leaf>,
}
#[derive(Deserialize)]
struct Leaf {
    function: u32,
    index: u32,
    eax: u32,
    ebx: u32,
    ecx: u32,
    edx: u32,
}

pub(crate) fn skylake_cpuid(id: u8, count: u32) -> Result<CpuId, VmmError> {
    let capture: Capture = serde_json::from_str(include_str!(
        "../../../docs/hwref/q35-macos-tahoe/cpuid.json"
    ))
    .map_err(|e| VmmError::Other(format!("invalid pinned Skylake CPUID: {e}")))?;
    let shift = count.next_power_of_two().trailing_zeros();
    let mut entries: Vec<_> = capture
        .leaves
        .into_iter()
        .map(|leaf| kvm_cpuid_entry2 {
            function: leaf.function,
            index: leaf.index,
            eax: leaf.eax,
            ebx: leaf.ebx,
            ecx: leaf.ecx,
            edx: leaf.edx,
            flags: if matches!(leaf.function, 4 | 7 | 0xb | 0xd) {
                kvm_bindings::KVM_CPUID_FLAG_SIGNIFCANT_INDEX
            } else {
                0
            },
            ..Default::default()
        })
        .collect();
    for entry in &mut entries {
        match entry.function {
            1 => {
                entry.ebx = (entry.ebx & 0xffff) | (count << 16) | (u32::from(id) << 24);
                if count > 1 {
                    entry.edx |= 1 << 28;
                } else {
                    entry.edx &= !(1 << 28);
                }
                // KVM supplies OSXSAVE from the guest's CR4 at execution time.
                entry.ecx &= !(1 << 27);
            }
            4 if entry.eax & 31 != 0 => {
                let sharing = if (entry.eax >> 5) & 7 == 3 {
                    (count - 1) << 14
                } else {
                    0
                };
                entry.eax = (entry.eax & 0x3fff) | sharing | ((count.min(64) - 1) << 26);
            }
            0xb => {
                let (width, cpus, level) = match entry.index {
                    0 => (0, 1, 1),
                    1 => (shift, count, 2),
                    _ => (0, 0, 0),
                };
                entry.eax = width;
                entry.ebx = cpus;
                entry.ecx = level << 8 | entry.index;
                entry.edx = u32::from(id);
            }
            _ => {}
        }
    }
    CpuId::from_entries(&entries).map_err(|e| VmmError::Other(format!("Skylake CPUID: {e}")))
}

pub(crate) fn configure_tsc(
    cpu: &VcpuFd,
    desc: &MachineDescription,
    report: bool,
) -> Result<(), VmmError> {
    if let Some(khz) = desc.tsc_khz {
        cpu.set_tsc_khz(khz)
            .map_err(|e| VmmError::ioctl("KVM_SET_TSC_KHZ", e))?;
    }
    if !desc.invtsc
        && !desc.vmware_cpuid_freq
        && desc.tsc_khz.is_none()
        && desc.cpu_model.as_deref() != Some("Skylake-Client-v4")
    {
        return Ok(());
    }
    let khz = cpu
        .get_tsc_khz()
        .map_err(|e| VmmError::ioctl("KVM_GET_TSC_KHZ", e))?;
    if desc.tsc_khz.is_some_and(|pin| pin != khz) {
        return Err(VmmError::Other(
            "KVM did not retain the requested TSC frequency".into(),
        ));
    }
    if report {
        eprintln!("vmlab-hv: tsc_khz={khz}");
    }
    let mut entries = cpu
        .get_cpuid2(KVM_MAX_CPUID_ENTRIES)
        .map_err(|e| VmmError::ioctl("KVM_GET_CPUID2", e))?
        .as_slice()
        .to_vec();
    for entry in &mut entries {
        if entry.function == 1 && desc.vmware_cpuid_freq {
            entry.ecx |= 1 << 31;
        }
        if entry.function == 0x80000007 {
            entry.edx = (entry.edx & !(1 << 8)) | (u32::from(desc.invtsc) << 8);
        }
        if entry.function == 0x40000000 {
            entry.eax = if desc.vmware_cpuid_freq {
                0x40000010
            } else {
                0x40000001
            };
        }
    }
    entries.retain(|entry| entry.function != 0x40000010);
    if desc.vmware_cpuid_freq {
        entries.push(kvm_cpuid_entry2 {
            function: 0x40000010,
            eax: khz,
            ebx: APIC_BUS_KHZ,
            ..Default::default()
        });
    }
    let cpuid = CpuId::from_entries(&entries).map_err(|e| VmmError::Other(e.to_string()))?;
    cpu.set_cpuid2(&cpuid)
        .map_err(|e| VmmError::ioctl("KVM_SET_CPUID2", e))
}

pub(crate) fn configure_msrs(kvm: &KvmVm, policy: MsrPolicy) -> Result<(), VmmError> {
    if policy == MsrPolicy::Fault {
        return Ok(());
    }
    if !kvm.kvm.check_extension(Cap::X86UserSpaceMsr) {
        return Err(VmmError::Other(
            "msr_policy=ignore requires KVM_CAP_X86_USER_SPACE_MSR".into(),
        ));
    }
    kvm.vm
        .enable_cap(&kvm_enable_cap {
            cap: Cap::X86UserSpaceMsr as u32,
            args: [u64::from(MsrExitReason::Unknown.bits()), 0, 0, 0],
            ..Default::default()
        })
        .map_err(|e| VmmError::ioctl("KVM_ENABLE_CAP userspace MSRs", e))
}

pub(crate) fn ignored_msr(shared: &crate::vm::VmShared, index: u32, reason: MsrExitReason) -> bool {
    let (allowed, first) = msr_action(
        *shared.msr_policy.lock(),
        &mut shared.ignored_msrs.lock(),
        index,
        reason,
    );
    if !allowed {
        return false;
    }
    if first {
        eprintln!("kvm: ignored unknown MSR {index:#010x}");
    }
    true
}

fn msr_action(
    policy: MsrPolicy,
    seen: &mut std::collections::BTreeSet<u32>,
    index: u32,
    reason: MsrExitReason,
) -> (bool, bool) {
    if policy != MsrPolicy::Ignore || reason != MsrExitReason::Unknown {
        return (false, false);
    }
    (true, seen.insert(index))
}

#[cfg(test)]
mod tests {
    use super::*;
    use vm_memory::{Bytes, GuestAddress};

    #[test]
    fn cpuid_matches_every_captured_leaf_with_documented_dynamic_fields() {
        let capture: Capture = serde_json::from_str(include_str!(
            "../../../docs/hwref/q35-macos-tahoe/cpuid.json"
        ))
        .unwrap();
        let actual = skylake_cpuid(0, 4).unwrap();
        for expected in capture.leaves {
            let entry = actual
                .as_slice()
                .iter()
                .find(|entry| entry.function == expected.function && entry.index == expected.index)
                .unwrap();
            let ecx = if expected.function == 1 {
                expected.ecx & !(1 << 27)
            } else {
                expected.ecx
            };
            assert_eq!(
                (entry.eax, entry.ebx, entry.ecx, entry.edx),
                (expected.eax, expected.ebx, ecx, expected.edx),
                "leaf {:#x}/{}",
                expected.function,
                expected.index
            );
        }
    }

    #[test]
    fn cpuid_topology_tracks_the_requested_vcpu_count_and_apic_id() {
        for count in [1, 2, 4, 8] {
            for id in 0..count {
                let cpuid = skylake_cpuid(id as u8, count).unwrap();
                let leaf = |function, index| {
                    cpuid
                        .as_slice()
                        .iter()
                        .find(|entry| entry.function == function && entry.index == index)
                        .unwrap()
                };
                assert_eq!(leaf(1, 0).ebx >> 24, id);
                assert_eq!((leaf(1, 0).ebx >> 16) & 255, count);
                assert_eq!(leaf(1, 0).edx & (1 << 28) != 0, count > 1);
                assert_eq!(leaf(0xb, 1).ebx, count);
                assert_eq!(leaf(0xb, 1).edx, id);
                assert_eq!(leaf(0xb, 1).eax, count.trailing_zeros());
            }
        }
    }

    #[test]
    fn msr_policy_logs_each_index_once_and_preserves_faults() {
        let mut seen = Default::default();
        for reason in [
            MsrExitReason::Unknown,
            MsrExitReason::Inval,
            MsrExitReason::Filter,
        ] {
            assert_eq!(
                msr_action(MsrPolicy::Fault, &mut seen, 0xdeadbeef, reason),
                (false, false)
            );
        }
        assert_eq!(
            msr_action(
                MsrPolicy::Ignore,
                &mut seen,
                0xdeadbeef,
                MsrExitReason::Unknown
            ),
            (true, true)
        );
        assert_eq!(
            msr_action(
                MsrPolicy::Ignore,
                &mut seen,
                0xdeadbeef,
                MsrExitReason::Unknown
            ),
            (true, false)
        );
        assert_eq!(
            msr_action(
                MsrPolicy::Ignore,
                &mut seen,
                0xdeadbeef,
                MsrExitReason::Inval
            ),
            (false, false)
        );
        let saved = serde_json::to_vec(&seen).unwrap();
        let mut restored = serde_json::from_slice(&saved).unwrap();
        assert_eq!(
            msr_action(
                MsrPolicy::Ignore,
                &mut restored,
                0xdeadbeef,
                MsrExitReason::Unknown
            ),
            (true, false)
        );
    }

    #[test]
    #[cfg_attr(vmlab_ci_no_kvm, ignore = "requires KVM")]
    fn kvm_unknown_msr_exits_return_zero_drop_writes_and_complete() {
        if std::fs::read_to_string("/sys/module/kvm/parameters/ignore_msrs")
            .is_ok_and(|value| value.trim() == "Y")
        {
            eprintln!(
                "skipping unknown-MSR KVM exits: host ignore_msrs=Y suppresses those exits; rerun with ignore_msrs=N"
            );
            return;
        }
        if !std::path::Path::new("/dev/kvm").exists() {
            eprintln!("skipping MSR exits: /dev/kvm unavailable");
            return;
        }
        let vm = KvmVm::new().unwrap();
        configure_msrs(&vm, MsrPolicy::Ignore).unwrap();
        let _permit = vmlab_hv_tests::process::VmPermit::acquire().unwrap();
        let ram =
            crate::memory::GuestRam::anonymous(crate::layout::MemLayout::new(16).unwrap()).unwrap();
        crate::memory::register_memslots(&vm.vm, &ram).unwrap();
        let code = [
            0x66, 0xb9, 0xef, 0xbe, 0xad, 0xde, 0x0f, 0x32, 0x66, 0xa3, 0, 0x90, 0x66, 0xb8, 0x12,
            0x34, 0x56, 0x78, 0x0f, 0x30, 0x0f, 0x32, 0x66, 0xa3, 4, 0x90, 0xe6, 0xe9,
        ];
        ram.mem.write_slice(&code, GuestAddress(0x8000)).unwrap();
        let mut cpu = vm.create_vcpu(0).unwrap();
        crate::loader::init_real_mode(&mut cpu, 0x8000).unwrap();
        let mut seen = Default::default();
        for write in [false, true, false] {
            match cpu.run().unwrap() {
                kvm_ioctls::VcpuExit::X86Rdmsr(exit) if !write => {
                    assert!(msr_action(MsrPolicy::Ignore, &mut seen, exit.index, exit.reason).0);
                    *exit.error = 0;
                    *exit.data = 0;
                }
                kvm_ioctls::VcpuExit::X86Wrmsr(exit) if write => {
                    assert_eq!(exit.data, 0x78563412);
                    assert!(msr_action(MsrPolicy::Ignore, &mut seen, exit.index, exit.reason).0);
                    *exit.error = 0;
                }
                other => panic!("unexpected MSR test exit: {other:?}"),
            }
        }
        assert!(matches!(
            cpu.run().unwrap(),
            kvm_ioctls::VcpuExit::IoOut(0xe9, _)
        ));
        let mut bytes = [0xff; 8];
        ram.mem
            .read_slice(&mut bytes, GuestAddress(0x9000))
            .unwrap();
        assert_eq!(bytes, [0; 8]);
        assert_eq!(seen.len(), 1);
    }

    #[test]
    #[cfg_attr(vmlab_ci_no_kvm, ignore = "requires KVM")]
    fn frequency_leaf_uses_the_pinned_kvm_frequency() {
        if !std::path::Path::new("/dev/kvm").exists() {
            eprintln!("skipping TSC: /dev/kvm unavailable");
            return;
        }
        let vm = KvmVm::new().unwrap();
        let cpu = vm
            .create_vcpu_with_model(
                0,
                4,
                vmlab_hv_proto::machine::CpuPolicy::Host,
                Some("Skylake-Client-v4"),
            )
            .unwrap();
        let khz = cpu.get_tsc_khz().unwrap() / 2;
        let desc: MachineDescription = serde_json::from_value(serde_json::json!({
            "name":"tsc", "arch":"x86_64", "accel":"kvm", "vcpus":4, "memory_mib":16,
            "boot":{"kind":"kernel","kernel":"unused","cmdline":""},
            "disks":[],"nics":[],"ports":[],"serial_log":"unused",
            "cpu_model":"Skylake-Client-v4","invtsc":true,"vmware_cpuid_freq":true,"tsc_khz":khz
        }))
        .unwrap();
        configure_tsc(&cpu, &desc, true).unwrap();
        let cpuid = cpu.get_cpuid2(KVM_MAX_CPUID_ENTRIES).unwrap();
        let leaf = cpuid
            .as_slice()
            .iter()
            .find(|entry| entry.function == 0x40000010)
            .unwrap();
        assert_eq!((leaf.eax, leaf.ebx), (khz, 1_000_000));
        assert_ne!(
            cpuid
                .as_slice()
                .iter()
                .find(|entry| entry.function == 0x80000007)
                .unwrap()
                .edx
                & 256,
            0
        );
    }

    #[test]
    #[cfg_attr(vmlab_ci_no_kvm, ignore = "requires KVM")]
    fn kvm_fault_policy_delivers_unknown_msr_gp_to_the_guest() {
        if !std::path::Path::new("/dev/kvm").exists()
            || std::fs::read_to_string("/sys/module/kvm/parameters/ignore_msrs")
                .is_ok_and(|value| value.trim() != "N")
        {
            eprintln!("skipping fault-policy guest: requires /dev/kvm and ignore_msrs=N");
            return;
        }
        let _permit = vmlab_hv_tests::process::VmPermit::acquire().unwrap();
        let vm = KvmVm::new().unwrap();
        configure_msrs(&vm, MsrPolicy::Fault).unwrap();
        let ram =
            crate::memory::GuestRam::anonymous(crate::layout::MemLayout::new(16).unwrap()).unwrap();
        crate::memory::register_memslots(&vm.vm, &ram).unwrap();
        ram.mem
            .write_slice(
                &[0x66, 0xb9, 0xef, 0xbe, 0xad, 0xde, 0x0f, 0x32, 0xf4],
                GuestAddress(0x8000),
            )
            .unwrap();
        ram.mem
            .write_slice(&[0, 0x81, 0, 0], GuestAddress(13 * 4))
            .unwrap();
        ram.mem
            .write_slice(&[0xb0, 0x42, 0xe6, 0xe9], GuestAddress(0x8100))
            .unwrap();
        let mut cpu = vm.create_vcpu(0).unwrap();
        crate::loader::init_real_mode(&mut cpu, 0x8000).unwrap();
        match cpu.run().unwrap() {
            kvm_ioctls::VcpuExit::IoOut(0xe9, bytes) => assert_eq!(bytes, [0x42]),
            other => panic!("unknown MSR did not enter the guest #GP handler: {other:?}"),
        }
    }
}
