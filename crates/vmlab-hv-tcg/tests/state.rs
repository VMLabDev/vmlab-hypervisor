use vmlab_hv_tcg::{FlatMemory, Memory, Riscv64, SharedMemory};

#[test]
fn snapshot_restores_architecture_and_rebuilds_translation_cache() {
    let mut memory = SharedMemory::new(FlatMemory::new(0x80000000, vec![0; 4096]));
    memory
        .write(0x80000000, &0x00108093u32.to_le_bytes())
        .unwrap();
    memory
        .write(0x80000004, &0xffdff06fu32.to_le_bytes())
        .unwrap();
    let mut cpu = Riscv64::new(memory).unwrap();
    cpu.specialize = true;
    cpu.interpreter.operation_limit = u64::MAX;
    cpu.set_pc(0x80000000);
    cpu.run(64).unwrap();
    assert!(cpu.specialization_stats.hits > 0);
    let state = cpu.save_state();
    let pc = cpu.pc().unwrap();
    let x1 = cpu.gpr(1).unwrap();
    let steps = cpu.steps;
    cpu.run(64).unwrap();
    assert!(cpu.gpr(1).unwrap() > x1);
    cpu.restore_state(&state).unwrap();
    assert_eq!(cpu.pc().unwrap(), pc);
    assert_eq!(cpu.gpr(1).unwrap(), x1);
    assert_eq!(cpu.steps, steps);
    assert!(cpu.specialize);
    assert_eq!(cpu.interpreter.operation_limit, u64::MAX);
    assert!(cpu.trace_profile().is_empty());
    cpu.run(64).unwrap();
    assert_eq!(cpu.gpr(1).unwrap(), x1 + 32);
}

#[test]
fn external_timer_is_not_advanced_by_retirement_and_survives_restore() {
    let mut memory = SharedMemory::new(FlatMemory::new(0x80000000, vec![0; 4096]));
    for (offset, word) in [0xc01021f3u32, 0x00108093, 0xff9ff06f]
        .into_iter()
        .enumerate()
    {
        memory
            .write(0x80000000 + offset as u64 * 4, &word.to_le_bytes())
            .unwrap();
    }
    let mut cpu = Riscv64::new(memory).unwrap();
    cpu.use_external_clock().unwrap();
    cpu.specialize = true;
    cpu.set_pc(0x80000000);
    cpu.interpreter
        .globals
        .insert("zmtime".into(), vmlab_hv_tcg::Value::bits(64, 123456u64));
    cpu.run(96).unwrap();
    assert_eq!(cpu.gpr(3).unwrap(), 123456);
    assert_eq!(cpu.interpreter.globals["zmtime"].u64().unwrap(), 123456);
    assert_eq!(cpu.interpreter.globals["zmcycle"].u64().unwrap(), 96);
    assert!(cpu.specialization_stats.hits > 0);
    let state = cpu.save_state();
    cpu.run(96).unwrap();
    cpu.restore_state(&state).unwrap();
    cpu.run(96).unwrap();
    assert_eq!(cpu.interpreter.globals["zmtime"].u64().unwrap(), 123456);
    assert_eq!(cpu.interpreter.globals["zmcycle"].u64().unwrap(), 192);
}
