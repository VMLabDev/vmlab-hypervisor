use vmlab_hv_tcg::{FlatMemory, Memory, Riscv64, Value};

fn loop_cpu() -> Riscv64<FlatMemory> {
    let mut memory = FlatMemory::new(0x80000000, vec![0; 4096]);
    for (index, word) in [0x00108093u32, 0xffdff06f].into_iter().enumerate() {
        memory
            .write(0x80000000 + index as u64 * 4, &word.to_le_bytes())
            .unwrap();
    }
    let mut cpu = Riscv64::new(memory).unwrap();
    cpu.use_external_clock().unwrap();
    cpu.set_pc(0x80000000);
    cpu.interpreter.operation_limit = u64::MAX;
    cpu.interpreter
        .globals
        .insert("zmtime".into(), Value::bits(64, 1000u64));
    cpu
}

#[test]
fn changing_future_machine_timer_deadlines_does_not_retrain_warm_code() {
    let mut reference = loop_cpu();
    let mut specialized = loop_cpu();
    specialized.specialize = true;
    for cpu in [&mut reference, &mut specialized] {
        cpu.interpreter
            .globals
            .insert("zmtimecmp".into(), Value::bits(64, 1_000_000u64));
        cpu.run(4096).unwrap();
    }
    let recordings = specialized.specialization_stats.recordings;
    for deadline in 1_000_001u64..1_000_017 {
        for cpu in [&mut reference, &mut specialized] {
            cpu.interpreter
                .globals
                .insert("zmtimecmp".into(), Value::bits(64, deadline));
            cpu.run(128).unwrap();
        }
        assert_eq!(
            *specialized.interpreter.globals,
            *reference.interpreter.globals
        );
        assert_eq!(
            specialized.specialization_stats.recordings, recordings,
            "changing a future timer deadline retrained code"
        );
    }
    for cpu in [&mut reference, &mut specialized] {
        cpu.interpreter
            .globals
            .insert("zmtimecmp".into(), Value::bits(64, 500u64));
        cpu.run(1).unwrap();
    }
    assert_eq!(
        *specialized.interpreter.globals,
        *reference.interpreter.globals
    );
    assert_ne!(specialized.read_csr(0x344).unwrap() & (1 << 7), 0);
}

#[test]
fn changing_supervisor_timer_deadlines_preserves_native_code_and_pending_bits() {
    let mut reference = loop_cpu();
    let mut specialized = loop_cpu();
    specialized.specialize = true;
    for cpu in [&mut reference, &mut specialized] {
        cpu.write_csr(0x30a, 1 << 63).unwrap();
        assert_ne!(cpu.read_csr(0x30a).unwrap() & (1 << 63), 0);
        cpu.write_csr(0x14d, 1_000_000).unwrap();
        cpu.run(4096).unwrap();
    }
    let recordings = specialized.specialization_stats.recordings;
    for deadline in 1_000_001..1_000_017 {
        for cpu in [&mut reference, &mut specialized] {
            cpu.write_csr(0x14d, deadline).unwrap();
            cpu.run(128).unwrap();
        }
        assert_eq!(
            *specialized.interpreter.globals,
            *reference.interpreter.globals
        );
        assert_eq!(specialized.specialization_stats.recordings, recordings);
    }
    for deadline in [500, 1_000_100, 500, 1_000_200] {
        for cpu in [&mut reference, &mut specialized] {
            cpu.write_csr(0x14d, deadline).unwrap();
            cpu.run(128).unwrap();
        }
        assert_eq!(
            *specialized.interpreter.globals,
            *reference.interpreter.globals
        );
        assert_eq!(
            specialized.read_csr(0x344).unwrap() & (1 << 5) != 0,
            deadline < 1000
        );
    }
}

#[test]
fn trap_data_csrs_are_runtime_values_on_platform_cpus() {
    for csr in [0x340u32, 0x140, 0x341, 0x141, 0x342, 0x142, 0x343, 0x143] {
        let cause = matches!(csr, 0x342 | 0x142);
        let initial = if cause { 0 } else { 0x80001000 };
        let mut reference = loop_cpu();
        let mut specialized = loop_cpu();
        specialized.specialize = true;
        for cpu in [&mut reference, &mut specialized] {
            let read = csr << 20 | 2 << 12 | 10 << 7 | 0x73;
            cpu.interpreter
                .memory
                .write(0x80000000, &read.to_le_bytes())
                .unwrap();
            cpu.write_csr(csr as u16, initial).unwrap();
            cpu.run(4096).unwrap();
        }
        let recordings = specialized.specialization_stats.recordings;
        for index in 1..9 {
            let value = if cause { index } else { initial + index * 4 };
            for cpu in [&mut reference, &mut specialized] {
                cpu.write_csr(csr as u16, value).unwrap();
                cpu.run(128).unwrap();
            }
            assert_eq!(
                *specialized.interpreter.globals, *reference.interpreter.globals,
                "CSR {csr:#x}"
            );
            assert_eq!(
                specialized.gpr(10).unwrap(),
                reference.read_csr(csr as u16).unwrap()
            );
            assert_eq!(specialized.gpr(10).unwrap(), value);
            assert_eq!(
                specialized.specialization_stats.recordings, recordings,
                "CSR {csr:#x} retrained code"
            );
        }
    }
}
