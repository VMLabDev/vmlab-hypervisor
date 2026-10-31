use vmlab_hv_tcg::{FlatMemory, Memory, Privilege, Riscv64, Value};

fn cpu(words: &[u32]) -> Riscv64<FlatMemory> {
    let mut memory = FlatMemory::new(0x8000_0000, vec![0; 64 * 1024]);
    for (i, instruction) in words.iter().enumerate() {
        memory
            .write(0x8000_0000 + i as u64 * 4, &instruction.to_le_bytes())
            .unwrap();
    }
    let mut cpu = Riscv64::new(memory).unwrap();
    cpu.set_pc(0x8000_0000);
    cpu
}

#[test]
fn generated_fetch_decode_execute_and_self_modification() {
    let mut cpu = cpu(&[0x00500093, 0x00708113]); // addi x1,zero,5; addi x2,x1,7
    cpu.step().unwrap();
    cpu.step().unwrap();
    assert_eq!(cpu.gpr(1).unwrap(), 5);
    assert_eq!(cpu.gpr(2).unwrap(), 12);
    assert_eq!(cpu.read_csr(0xb00).unwrap(), 2);
    cpu.interpreter
        .memory
        .write(0x8000_0000, &0x00900093u32.to_le_bytes())
        .unwrap();
    cpu.set_pc(0x8000_0000);
    cpu.step().unwrap();
    assert_eq!(cpu.gpr(1).unwrap(), 9);
    assert_eq!(cpu.gpr(0).unwrap(), 0);
    assert_eq!(cpu.pc().unwrap(), 0x8000_0004);
}

#[test]
fn decoder_cache_observes_floating_point_enable_changes() {
    let mut cpu = cpu(&[0x022081d3]); // fadd.d f3,f1,f2
    cpu.write_csr(0x305, 0x8000_0100).unwrap();
    cpu.step().unwrap();
    assert_eq!(cpu.read_csr(0x342).unwrap(), 2);
    let status = cpu.read_csr(0x300).unwrap();
    cpu.write_csr(0x300, status | 0x6000).unwrap();
    cpu.interpreter
        .globals
        .insert("zf1".into(), Value::bits(64, 1.5f64.to_bits()));
    cpu.interpreter
        .globals
        .insert("zf2".into(), Value::bits(64, 2.25f64.to_bits()));
    cpu.set_pc(0x8000_0000);
    cpu.step().unwrap();
    assert_eq!(
        cpu.interpreter.globals["zf3"].u64().unwrap(),
        3.75f64.to_bits()
    );
}

#[test]
fn generated_csr_lookup_and_constructor_mismatch() {
    let mut cpu = cpu(&[]);
    let result = cpu
        .interpreter
        .call(
            "zcsr_name_map_backwards",
            vec![Value::String("mstatus".into())],
        )
        .unwrap();
    assert_eq!(result.u64().unwrap(), 0x300);
    assert_eq!(cpu.read_csr(0x301).unwrap() >> 62, 2);
    for csr in [
        0x001u16, 0x002, 0x003, 0x100, 0x104, 0x105, 0x106, 0x140, 0x141, 0x142, 0x143, 0x144,
        0x180, 0x300, 0x301, 0x302, 0x303, 0x304, 0x305, 0x306, 0x340, 0x341, 0x342, 0x343, 0x344,
        0xb00, 0xb02,
    ] {
        let original = &cpu.interpreter.program.functions["zread_CSR"];
        let full = cpu
            .interpreter
            .run_function(original, vec![Value::bits(12, csr)])
            .unwrap()
            .u64()
            .unwrap();
        assert_eq!(cpu.read_csr(csr).unwrap(), full, "CSR {csr:#x}");
    }
}

#[test]
fn generated_load_store_and_multiply() {
    let mut cpu = cpu(&[0x0020b023, 0x0000b183, 0x02218233]); // sd x2,0(x1); ld x3,0(x1); mul x4,x3,x2
    cpu.set_gpr(1, 0x8000_1000).unwrap();
    cpu.set_gpr(2, 13).unwrap();
    for _ in 0..3 {
        cpu.step().unwrap();
    }
    assert_eq!(cpu.gpr(3).unwrap(), 13);
    assert_eq!(cpu.gpr(4).unwrap(), 169);
}

#[test]
fn generated_compressed_and_machine_trap() {
    let mut cpu = cpu(&[0x00000085, 0x00000073]); // c.addi x1,1; invalid compressed halfword; ecall
    cpu.step().unwrap();
    assert_eq!(cpu.gpr(1).unwrap(), 1);
    assert_eq!(cpu.pc().unwrap(), 0x8000_0002);
    cpu.set_pc(0x8000_0004);
    cpu.write_csr(0x305, 0x8000_0100).unwrap();
    cpu.step().unwrap();
    assert_eq!(cpu.pc().unwrap(), 0x8000_0100);
    assert_eq!(cpu.read_csr(0x342).unwrap(), 11);
    assert_eq!(cpu.read_csr(0x341).unwrap(), 0x8000_0004);
}

#[test]
fn softfloat_rounding_flags_and_nan() {
    let mut cpu = cpu(&[]);
    let result = cpu
        .interpreter
        .call(
            "zriscv_f32Div",
            vec![
                Value::bits(3, 0u8),
                Value::bits(32, 0x3f800000u32),
                Value::bits(32, 0u8),
            ],
        )
        .unwrap();
    let Value::Struct(fields) = result else {
        panic!("tuple expected");
    };
    let values: Vec<_> = fields.values().map(|v| v.u64().unwrap()).collect();
    assert!(values.contains(&8));
    assert!(values.contains(&0x7f800000));
    let result = cpu
        .interpreter
        .call(
            "zriscv_f32Add",
            vec![
                Value::bits(3, 0u8),
                Value::bits(32, 0x7f800001u32),
                Value::bits(32, 0x3f800000u32),
            ],
        )
        .unwrap();
    let Value::Struct(fields) = result else {
        panic!("tuple expected");
    };
    let values: Vec<_> = fields.values().map(|v| v.u64().unwrap()).collect();
    assert!(values.contains(&16));
    assert!(values.contains(&0x7fc00000));
}

fn paged(mode: u64, user: bool) -> Riscv64<FlatMemory> {
    let mut cpu = cpu(&[0x00500093]);
    let levels = match mode {
        8 => 3,
        9 => 4,
        10 => 5,
        _ => panic!("unsupported paging mode"),
    };
    for level in 1..levels {
        let next = 0x8000_0000 + (level + 1) * 4096;
        let pte = ((next >> 12) << 10) | 1;
        cpu.interpreter
            .memory
            .write(0x8000_0000 + level * 4096, &pte.to_le_bytes())
            .unwrap();
    }
    let pte = (0x80000u64 << 10) | 0xcf | if user { 0x10 } else { 0 };
    cpu.interpreter
        .memory
        .write(0x8000_0000 + levels * 4096 + 4 * 8, &pte.to_le_bytes())
        .unwrap();
    cpu.write_csr(0x3b0, (1u64 << 54) - 1).unwrap();
    cpu.write_csr(0x3a0, 0x1f).unwrap();
    cpu.write_csr(0x180, (mode << 60) | 0x80001).unwrap();
    cpu.write_csr(0x305, 0x8000_0100).unwrap();
    cpu.set_privilege(Privilege::User);
    cpu.set_pc(0x4000);
    cpu
}

#[test]
fn sail_sv39_sv48_page_walks_and_tlb() {
    for mode in [8, 9] {
        let mut cpu = paged(mode, true);
        cpu.step().unwrap();
        assert_eq!(cpu.gpr(1).unwrap(), 5, "mode {mode}");
        assert_eq!(cpu.pc().unwrap(), 0x4004, "mode {mode}");
        cpu.set_pc(0x4000);
        cpu.step().unwrap();
        assert_eq!(cpu.pc().unwrap(), 0x4004);
    }
}

#[test]
fn unrelated_tlb_entries_do_not_retrain_a_paged_instruction_loop() {
    for mode in [8, 9, 10] {
        unused_tlb_entries(mode);
    }
}

fn unused_tlb_entries(mode: u64) {
    let leaf_table = 0x8000_0000 + (mode - 5) * 4096;
    let make = || {
        let mut cpu = paged(mode, true);
        for (index, instruction) in [0x00108093u32, 0xffdff06f].into_iter().enumerate() {
            cpu.interpreter
                .memory
                .write(0x8000_0000 + index as u64 * 4, &instruction.to_le_bytes())
                .unwrap();
        }
        for index in 0..8 {
            let vpn = 63 + index * 64;
            let pte = (0x80000u64 << 10) | 0xdf;
            cpu.interpreter
                .memory
                .write(leaf_table + vpn * 8, &pte.to_le_bytes())
                .unwrap();
        }
        cpu.set_gpr(1, 0).unwrap();
        cpu.interpreter.operation_limit = u64::MAX;
        cpu
    };
    let mut reference = make();
    let mut specialized = make();
    specialized.specialize = true;
    let Value::Vector(empty_tlb) = reference.interpreter.globals["ztlb"].clone() else {
        panic!("TLB vector expected");
    };
    for _ in 0..4 {
        reference.run(64).unwrap();
        specialized.run(64).unwrap();
    }
    let trained = specialized.specialization_stats.recordings;
    let mut filled = None;
    for index in 0..8 {
        for cpu in [&mut reference, &mut specialized] {
            let Value::Vector(entries) = &cpu.interpreter.globals["ztlb"] else {
                panic!("TLB vector expected");
            };
            let Value::Union(_, entry) = entries[4].clone() else {
                panic!("cached code translation expected");
            };
            let Value::Struct(mut fields) = *entry else {
                panic!("TLB entry expected");
            };
            if index % 2 == 1 {
                let Value::Vector(mut entries) = cpu.interpreter.globals["ztlb"].clone() else {
                    panic!("TLB vector expected");
                };
                entries[63] = empty_tlb[63].clone();
                cpu.interpreter
                    .globals
                    .insert("ztlb".into(), Value::Vector(entries));
                continue;
            }
            let vpn = 63 + index * 64;
            fields.insert("zvpn".into(), Value::bits(47, vpn));
            fields.insert("zpteAddr".into(), Value::bits(64, leaf_table + vpn * 8));
            cpu.interpreter
                .call("zwrite_TLB", vec![Value::int(63), Value::Struct(fields)])
                .unwrap();
        }
        reference.run(64).unwrap();
        specialized.run(64).unwrap();
        assert_eq!(
            *specialized.interpreter.globals,
            *reference.interpreter.globals
        );
        assert_eq!(specialized.pc().unwrap(), 0x4000);
        if let Some(recordings) = filled {
            assert_eq!(specialized.specialization_stats.recordings, recordings);
        } else {
            filled = Some(specialized.specialization_stats.recordings);
        }
    }
    let retrained = specialized.specialization_stats.recordings - trained;
    eprintln!("unused TLB entry replacement: {retrained} retrainings over 8 batches");
    assert_eq!(retrained, 0, "unrelated translations were retrained");
    let before = reference.gpr(1).unwrap();
    for cpu in [&mut reference, &mut specialized] {
        for (index, instruction) in [0x00208093u32, 0xffdff06f].into_iter().enumerate() {
            cpu.interpreter
                .memory
                .write(0x8000_8000 + index as u64 * 4, &instruction.to_le_bytes())
                .unwrap();
        }
        let pte = (0x80008u64 << 10) | 0xdf;
        cpu.interpreter
            .memory
            .write(leaf_table + 4 * 8, &pte.to_le_bytes())
            .unwrap();
        let Value::Vector(entries) = &cpu.interpreter.globals["ztlb"] else {
            panic!("TLB vector expected");
        };
        let Value::Union(_, entry) = entries[4].clone() else {
            panic!("cached code translation expected");
        };
        let Value::Struct(mut fields) = *entry else {
            panic!("TLB entry expected");
        };
        fields.insert("zppn".into(), Value::bits(44, 0x80008u64));
        fields.insert("zpte".into(), Value::bits(64, pte));
        cpu.interpreter
            .call("zwrite_TLB", vec![Value::int(4), Value::Struct(fields)])
            .unwrap();
    }
    reference.run(64).unwrap();
    specialized.run(64).unwrap();
    assert_eq!(specialized.gpr(1).unwrap(), before + 64);
    assert_eq!(
        *specialized.interpreter.globals,
        *reference.interpreter.globals
    );
}

#[test]
fn paged_data_loads_specialize_and_revalidate_the_tlb_index() {
    let make = || {
        let mut cpu = paged(8, true);
        for (index, instruction) in [0x00013183u32, 0x00108093, 0xff9ff06f]
            .into_iter()
            .enumerate()
        {
            cpu.interpreter
                .memory
                .write(0x8000_0000 + index as u64 * 4, &instruction.to_le_bytes())
                .unwrap();
        }
        for index in 0..8 {
            let pte = ((0x80008u64 + index) << 10) | 0xdf;
            cpu.interpreter
                .memory
                .write(0x8000_3000 + (16 + index) * 8, &pte.to_le_bytes())
                .unwrap();
            cpu.interpreter
                .memory
                .write(0x8000_8000 + index * 4096, &(42 + index).to_le_bytes())
                .unwrap();
        }
        cpu.set_gpr(1, 0).unwrap();
        cpu.set_gpr(2, 0x10000).unwrap();
        cpu.interpreter.operation_limit = u64::MAX;
        cpu
    };
    let mut reference = make();
    let mut specialized = make();
    specialized.set_trace_cache_limit(64_000_000).unwrap();
    specialized.specialize = true;
    for page in 0..8 {
        let address = 0x10000 + page * 4096;
        for cpu in [&mut reference, &mut specialized] {
            cpu.set_gpr(2, address).unwrap();
            for _ in 0..3 {
                cpu.run(192).unwrap();
            }
        }
        let before = specialized.interpreter.operations;
        reference.run(192).unwrap();
        specialized.run(192).unwrap();
        assert_eq!(
            *specialized.interpreter.globals,
            *reference.interpreter.globals
        );
        assert_eq!(specialized.gpr(3).unwrap(), 42 + (address - 0x10000) / 4096);
        assert_eq!(
            specialized.interpreter.operations, before,
            "paged load at {address:#x} returned to Sail"
        );
    }
    for turn in 0..48 {
        let page = turn % 8;
        let address = 0x10000 + page * 4096;
        let before = specialized.interpreter.operations;
        let recordings = specialized.specialization_stats.recordings;
        for cpu in [&mut reference, &mut specialized] {
            cpu.set_gpr(2, address).unwrap();
            cpu.run(192).unwrap();
        }
        assert_eq!(
            *specialized.interpreter.globals,
            *reference.interpreter.globals
        );
        if turn >= 24 {
            assert_eq!(
                specialized.interpreter.operations, before,
                "revisited page at {address:#x} returned to Sail"
            );
            assert_eq!(specialized.specialization_stats.recordings, recordings);
        }
        let (used, limit) = specialized.trace_cache_usage();
        assert!(used <= limit);
    }
    eprintln!(
        "paged load stops: {:?}",
        specialized.specialization_stats.stop_reasons
    );
    assert!(
        !specialized
            .specialization_stats
            .stop_reasons
            .keys()
            .any(|reason| reason.contains("vector_access"))
    );
    assert!(specialized.specialization_stats.hits > 192);
}

#[test]
fn sail_user_fetch_permission_fault() {
    let mut cpu = paged(8, false);
    cpu.step().unwrap();
    assert_eq!(cpu.pc().unwrap(), 0x8000_0100);
    assert_eq!(cpu.read_csr(0x342).unwrap(), 12);
    assert_eq!(cpu.read_csr(0x343).unwrap(), 0x4000);
}

#[test]
fn generated_lr_sc_observes_other_harts_store() {
    use vmlab_hv_tcg::SharedMemory;
    let mut memory = SharedMemory::new(FlatMemory::new(0x8000_0000, vec![0; 8192]));
    memory
        .write(0x8000_0000, &0x1000b12fu32.to_le_bytes())
        .unwrap(); // lr.d x2,(x1)
    memory
        .write(0x8000_0004, &0x1830b1afu32.to_le_bytes())
        .unwrap(); // sc.d x3,x3,(x1)
    let mut cpu = Riscv64::new(memory.clone()).unwrap();
    cpu.set_gpr(1, 0x8000_1000).unwrap();
    cpu.set_gpr(3, 9).unwrap();
    cpu.set_pc(0x8000_0000);
    cpu.step().unwrap();
    memory.write(0x8000_1000, &7u64.to_le_bytes()).unwrap();
    cpu.step().unwrap();
    assert_ne!(cpu.gpr(3).unwrap(), 0);
    let mut data = [0; 8];
    memory.read(0x8000_1000, &mut data).unwrap();
    assert_eq!(u64::from_le_bytes(data), 7);
}

#[test]
fn generated_amo_is_atomic_across_host_threads() {
    use vmlab_hv_tcg::SharedMemory;
    let mut memory = SharedMemory::new(FlatMemory::new(0x8000_0000, vec![0; 8192]));
    memory
        .write(0x8000_0000, &0x0020b02fu32.to_le_bytes())
        .unwrap(); // amoadd.d zero,x2,(x1)
    let mut handles = Vec::new();
    for _ in 0..2 {
        let ram = memory.clone();
        handles.push(std::thread::spawn(move || {
            let mut cpu = Riscv64::new(ram).unwrap();
            cpu.set_gpr(1, 0x8000_1000).unwrap();
            cpu.set_gpr(2, 1).unwrap();
            for _ in 0..100 {
                cpu.set_pc(0x8000_0000);
                cpu.step().unwrap();
            }
        }));
    }
    for handle in handles {
        handle.join().unwrap();
    }
    let mut data = [0; 8];
    memory.read(0x8000_1000, &mut data).unwrap();
    assert_eq!(u64::from_le_bytes(data), 200);
}
