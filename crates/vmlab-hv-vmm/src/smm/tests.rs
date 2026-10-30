use super::*;
use kvm_ioctls::VcpuExit;
use vmlab_hv_firmware::fw_cfg::FwCfg;

#[test]
fn ovmf_opens_tseg_independently_of_legacy_smram() {
    let mut control = SmramControl::new(0x80000000, 64).unwrap();
    control.write_config(0x9e, 7);
    control.write_config(0x9d, 8);
    assert!(!control.open());
    assert!(!control.tseg_open());
    control.write_config(0x9e, 6);
    assert!(!control.open());
    assert!(control.tseg_open());
    control.write_config(0x9d, 0x18);
    assert!(!control.tseg_open());
}

#[test]
fn default_smbase_query_lock_and_restore() {
    let mut control = SmramControl::new(0x80000000, 64).unwrap();
    assert_eq!(control.read_config(0x9c), Some(0));
    control.write_config(0x9c, 0xff);
    assert_eq!(control.read_config(0x9c), Some(1));
    assert!(control.default_smbase_open());
    control.write_config(0x9c, 2);
    assert!(!control.default_smbase_open());
    control.write_config(0x9c, 0xff);
    assert_eq!(control.read_config(0x9c), Some(2));
    let saved = control.save_state().unwrap();
    let mut restored = SmramControl::new(0x80000000, 64).unwrap();
    restored.restore_state(&saved).unwrap();
    assert!(!restored.default_smbase_open());
}

#[test]
fn kvm_tseg_alias_executes_and_snapshots_without_opening_legacy_smram() {
    let kvm = match crate::kvm::KvmVm::new() {
        Ok(kvm) => kvm,
        Err(error) => {
            eprintln!("skipping TSEG mapping: {error}");
            return;
        }
    };
    let mut smram = Smram::new(
        kvm.vm.clone(),
        &[(0xa0000, 0x20000), (0x100000, 0x1000)],
        30,
    )
    .unwrap();
    smram
        .memory()
        .write_slice(&[0xb0, 0x5a, 0xe6, 0xe9, 0xf4], GuestAddress(0x100000))
        .unwrap();
    smram.set_region_open(1, true).unwrap();
    let saved = smram.save_state().unwrap();
    smram.set_region_open(1, false).unwrap();
    smram.restore_state(&saved).unwrap();
    assert_eq!(smram.open, [false, true]);
    let mut vcpu = kvm.create_vcpu(0).unwrap();
    let mut sregs = vcpu.get_sregs().unwrap();
    sregs.cs.base = 0x100000;
    sregs.cs.selector = 0;
    vcpu.set_sregs(&sregs).unwrap();
    vcpu.set_regs(&kvm_bindings::kvm_regs {
        rip: 0,
        rflags: 2,
        ..Default::default()
    })
    .unwrap();
    match vcpu.run().unwrap() {
        VcpuExit::IoOut(0xe9, bytes) => assert_eq!(bytes, [0x5a]),
        exit => panic!("TSEG execution: {exit:?}"),
    }
    drop(vcpu);
}

#[test]
fn mch_lock_tseg_sizes_and_snapshot() {
    let mut control = SmramControl::new(0x80000000, 64).unwrap();
    assert!(!control.open());
    for (encoding, size) in [(0, 1), (2, 2), (4, 8), (6, 64)] {
        control.write_config(0x9e, encoding | 1);
        assert_eq!(control.tseg_size(), size << 20);
        assert_eq!(control.tseg_base(), 0x80000000 - (size << 20));
    }
    control.write_config(0x9d, 0x48);
    assert!(control.open());
    assert_eq!(
        control.ranges(),
        [
            (0x30000, 0x20000),
            (0xa0000, 0x20000),
            (0x7c000000, 64 << 20)
        ]
    );
    control.write_config(0x9d, 0x58);
    assert!(!control.open());
    let state = control.save_state().unwrap();
    control.write_config(0x9d, 0x48);
    control.write_config(0x9e, 0);
    assert_eq!(control.save_state().unwrap(), state);
    let mut restored = SmramControl::new(0x80000000, 64).unwrap();
    restored.restore_state(&state).unwrap();
    assert!(!restored.open());
    assert_eq!(restored.tseg_size(), 64 << 20);
}

#[test]
fn apmc_enable_status_lock_and_snapshot() {
    let mut control = SmiControl::default();
    assert!(!control.write(0xb2, 0x42));
    control.write(0x30, 0x23);
    control.write(0xb3, 0xab);
    assert!(control.write(0xb2, 0x42));
    assert_eq!(control.read(0x34), Some(0x20));
    control.write(0x34, 0x20);
    assert_eq!(control.read(0x34), Some(0));
    control.lock();
    control.write(0x30, 0x20);
    assert_eq!(control.read(0x30), Some(0x21));
    let state = control.save_state().unwrap();
    let mut restored = SmiControl::default();
    restored.restore_state(&state).unwrap();
    assert_eq!(restored.read(0xb3), Some(0xab));
    assert!(restored.write(0xb2, 0));
}

fn firmware() -> (SmiFwCfg, vmlab_hv_devices::GuestMemory) {
    let memory = GuestMemoryMmap::from_ranges(&[(GuestAddress(0), 0x10000)]).unwrap();
    (
        SmiFwCfg::new(FwCfg::new(memory.clone()), memory.clone()).unwrap(),
        memory,
    )
}

fn selector(fw: &mut SmiFwCfg, name: &str) -> u16 {
    fw.write(0, &0x19u16.to_le_bytes());
    let mut directory = vec![0; 4 + 3 * 64];
    for byte in &mut directory {
        fw.read(1, std::slice::from_mut(byte));
    }
    for entry in directory[4..].as_chunks::<64>().0 {
        let end = entry[8..].iter().position(|v| *v == 0).unwrap_or(56);
        if &entry[8..8 + end] == name.as_bytes() {
            return u16::from_be_bytes(entry[4..6].try_into().unwrap());
        }
    }
    panic!("missing {name}");
}

#[test]
fn edk2_stream_negotiation_rejection_lock_and_snapshot() {
    let (mut fw, _) = firmware();
    let request = selector(&mut fw, "etc/smi/requested-features");
    let ok = selector(&mut fw, "etc/smi/features-ok");
    for (features, expected) in [(8u64, 0), (1, 1), (0, 0), (1, 1)] {
        fw.write(0, &request.to_le_bytes());
        for byte in features.to_le_bytes() {
            fw.write(1, &[byte]);
        }
        fw.write(0, &ok.to_le_bytes());
        let mut result = [0];
        fw.read(1, &mut result);
        assert_eq!(result, [expected]);
    }
    assert!(fw.broadcast());
    let state = fw.save_state().unwrap();
    let (mut restored, _) = firmware();
    restored.restore_state(&state).unwrap();
    assert!(restored.broadcast());
}

fn dma(
    fw: &mut SmiFwCfg,
    memory: &vmlab_hv_devices::GuestMemory,
    key: u16,
    flags: u32,
    length: u32,
) {
    let mut descriptor = Vec::new();
    descriptor.extend(((u32::from(key) << 16) | 8 | flags).to_be_bytes());
    descriptor.extend(length.to_be_bytes());
    descriptor.extend(0x2000u64.to_be_bytes());
    memory
        .write_slice(&descriptor, GuestAddress(0x1000))
        .unwrap();
    fw.write(4, &0u32.to_be_bytes());
    fw.write(8, &0x1000u32.to_be_bytes());
    let mut completion = [0; 4];
    memory
        .read_slice(&mut completion, GuestAddress(0x1000))
        .unwrap();
    assert_eq!(completion, [0; 4]);
}

#[test]
fn edk2_dma_negotiation() {
    let (mut fw, memory) = firmware();
    let request = selector(&mut fw, "etc/smi/requested-features");
    let ok = selector(&mut fw, "etc/smi/features-ok");
    memory
        .write_slice(&7u64.to_le_bytes(), GuestAddress(0x2000))
        .unwrap();
    dma(&mut fw, &memory, request, 16, 8);
    dma(&mut fw, &memory, ok, 2, 1);
    let mut result = [0];
    memory
        .read_slice(&mut result, GuestAddress(0x2000))
        .unwrap();
    assert_eq!(result, [1]);
    assert!(fw.broadcast());
}

#[test]
fn kvm_smi_handler_writes_private_smram_and_returns_with_rsm() {
    let kvm = match crate::kvm::KvmVm::new() {
        Ok(kvm) => kvm,
        Err(e) => {
            eprintln!("skipping SMM execution: {e}");
            return;
        }
    };
    if let Err(e) = require_smm(&kvm.kvm) {
        eprintln!("skipping SMM execution: {e}");
        return;
    }
    let memory = GuestMemoryMmap::<()>::from_ranges(&[(GuestAddress(0), 0x50000)]).unwrap();
    for (space, size) in [(0, 0x50000), (SMM_ADDRESS_SPACE, 0x30000)] {
        let region = kvm_userspace_memory_region {
            slot: space,
            guest_phys_addr: 0,
            memory_size: size,
            userspace_addr: memory.get_host_address(GuestAddress(0)).unwrap() as u64,
            flags: 0,
        };
        // SAFETY: memory is page aligned and retained until the vCPU stops.
        unsafe { kvm.vm.set_user_memory_region(region) }.unwrap();
    }
    let mut smram = Smram::new(kvm.vm.clone(), &[(0x30000, 0x20000)], 10).unwrap();
    // CS override writes a marker in private SMRAM, exits for inspection, RSM.
    smram
        .memory()
        .write_slice(
            &[
                0x2e, 0xc6, 0x06, 0x00, 0x01, 0x5a, 0xb0, 0x7e, 0xe6, 0xe9, 0x0f, 0xaa,
            ],
            GuestAddress(0x38000),
        )
        .unwrap();
    // After RSM, read the same GPA from the normal address space and halt.
    memory
        .write_slice(
            &[
                0xb8, 0x00, 0x30, 0x8e, 0xd8, 0xa0, 0x00, 0x01, 0xe6, 0xe9, 0xf4,
            ],
            GuestAddress(0x1000),
        )
        .unwrap();
    let mut vcpu = kvm.create_vcpu(0).unwrap();
    let mut sregs = vcpu.get_sregs().unwrap();
    sregs.cs.base = 0;
    sregs.cs.selector = 0;
    vcpu.set_sregs(&sregs).unwrap();
    vcpu.set_regs(&kvm_bindings::kvm_regs {
        rip: 0x1000,
        rflags: 2,
        ..Default::default()
    })
    .unwrap();
    inject_smi(&vcpu).unwrap();
    match vcpu.run().unwrap() {
        VcpuExit::IoOut(0xe9, data) => assert_eq!(data, [0x7e]),
        other => panic!("SMM handler: {other:?}"),
    }
    assert!(in_smm(&vcpu).unwrap());
    let events = save_vcpu_events(&vcpu).unwrap();
    restore_vcpu_events(&vcpu, &events).unwrap();
    match vcpu.run().unwrap() {
        VcpuExit::IoOut(0xe9, data) => assert_eq!(data, [0]),
        other => panic!("RSM: {other:?}"),
    }
    assert!(!in_smm(&vcpu).unwrap());
    let mut marker = [0];
    smram
        .memory()
        .read_slice(&mut marker, GuestAddress(0x30100))
        .unwrap();
    assert_eq!(marker, [0x5a]);
    let state = smram.save_state().unwrap();
    smram
        .memory()
        .write_slice(&[0], GuestAddress(0x30100))
        .unwrap();
    smram.restore_state(&state).unwrap();
    smram
        .memory()
        .read_slice(&mut marker, GuestAddress(0x30100))
        .unwrap();
    assert_eq!(marker, [0x5a]);
    drop(vcpu);
    // Remove normal RAM before exercising the firmware's open/close alias.
    let removed = kvm_userspace_memory_region {
        slot: 0,
        ..Default::default()
    };
    // SAFETY: zero size unregisters a slot; vCPU is stopped.
    unsafe { kvm.vm.set_user_memory_region(removed) }.unwrap();
    smram.set_open(true).unwrap();
    smram.set_open(false).unwrap();
}
