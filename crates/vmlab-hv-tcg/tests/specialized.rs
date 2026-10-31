use vmlab_hv_tcg::{FlatMemory, Memory, Riscv64};

#[test]
fn specialized_loop_matches_all_sail_globals_and_revalidates_code() {
    let make = || {
        let mut memory = FlatMemory::new(0x8000_0000, vec![0; 4096]);
        for (i, instruction) in [0x00108093u32, 0xfff10113, 0xfe011ce3].iter().enumerate() {
            memory
                .write(0x8000_0000 + i as u64 * 4, &instruction.to_le_bytes())
                .unwrap();
        }
        let mut cpu = Riscv64::new(memory).unwrap();
        cpu.set_pc(0x8000_0000);
        cpu.set_gpr(2, 20).unwrap();
        cpu
    };
    let mut reference = make();
    let mut specialized = make();
    specialized.specialize = true;
    for step in 0..30 {
        if step == 15 {
            // Same address and width, different bytes: the old trace cannot run.
            for cpu in [&mut reference, &mut specialized] {
                cpu.interpreter
                    .memory
                    .write(0x8000_0000, &0x00308093u32.to_le_bytes())
                    .unwrap();
            }
        }
        assert_eq!(specialized.step().unwrap(), reference.step().unwrap());
        assert_eq!(
            *specialized.interpreter.globals, *reference.interpreter.globals,
            "step {step}"
        );
        assert_eq!(
            specialized.interpreter.memory.bytes(),
            reference.interpreter.memory.bytes()
        );
    }
    assert!(specialized.specialization_stats.hits > 20);
    assert!(specialized.specialization_stats.misses > 0);
}

#[test]
fn a_batched_trace_can_stop_at_every_guest_instruction_boundary() {
    let make = || {
        let mut memory = FlatMemory::new(0x8000_0000, vec![0; 4096]);
        for (i, instruction) in [0x00108093u32, 0xfff10113, 0xfe011ce3].iter().enumerate() {
            memory
                .write(0x8000_0000 + i as u64 * 4, &instruction.to_le_bytes())
                .unwrap();
        }
        let mut cpu = Riscv64::new(memory).unwrap();
        cpu.set_pc(0x8000_0000);
        cpu.set_gpr(2, 1000).unwrap();
        cpu
    };
    let mut reference = make();
    let mut specialized = make();
    specialized.specialize = true;
    // Train blocks, then use shorter prefixes and individual steps. Every
    // observed boundary includes counters and the full wide RVFI packet.
    for limit in [64, 64, 64, 1, 2, 3, 7, 17, 64, 1] {
        assert_eq!(
            specialized.run(limit).unwrap(),
            reference.run(limit).unwrap()
        );
        assert_eq!(specialized.steps, reference.steps);
        assert_eq!(
            *specialized.interpreter.globals, *reference.interpreter.globals,
            "limit {limit}"
        );
    }
    assert!(specialized.specialization_stats.hits > 64);
}

#[test]
fn a_native_store_watch_stops_after_the_store_before_the_next_instruction() {
    let mut memory = FlatMemory::new(0x8000_0000, vec![0; 8192]);
    memory
        .write(0x8000_0000, &0x0020b023u32.to_le_bytes())
        .unwrap();
    memory
        .write(0x8000_0004, &0x00118193u32.to_le_bytes())
        .unwrap();
    let mut cpu = Riscv64::new(memory).unwrap();
    cpu.specialize = true;
    cpu.set_pc(0x8000_0000);
    cpu.set_gpr(1, 0x8000_1000).unwrap();
    cpu.set_gpr(2, 13).unwrap();
    assert_eq!(cpu.run(2).unwrap(), (2, false));
    for value in [17, 31, u64::MAX, u64::MAX - 1] {
        cpu.set_pc(0x8000_0000);
        cpu.set_gpr(2, value).unwrap();
        cpu.set_gpr(3, 0).unwrap();
        assert_eq!(cpu.run_until_store(2, 0x8000_1003).unwrap(), (1, false));
        assert_eq!(cpu.pc().unwrap(), 0x8000_0004);
        assert_eq!(cpu.gpr(3).unwrap(), 0);
        assert_eq!(
            u64::from_le_bytes(
                cpu.interpreter.memory.bytes()[4096..4104]
                    .try_into()
                    .unwrap()
            ),
            value
        );
    }
    assert!(cpu.specialization_stats.hits >= 2);
}

#[test]
fn a_batch_fault_keeps_the_completed_instruction_count() {
    struct FailingStore {
        ram: FlatMemory,
        fail: bool,
        stores: usize,
    }
    impl Memory for FailingStore {
        fn read(&mut self, address: u64, bytes: &mut [u8]) -> vmlab_hv_tcg::Result<()> {
            self.ram.read(address, bytes)
        }
        fn write(&mut self, address: u64, bytes: &[u8]) -> vmlab_hv_tcg::Result<()> {
            self.stores += 1;
            if self.fail {
                return Err(vmlab_hv_tcg::Error::Memory(address));
            }
            self.ram.write(address, bytes)
        }
        fn generation(&self) -> u64 {
            self.ram.generation()
        }
        fn code_stamp(&self, address: u64, len: usize) -> Option<vmlab_hv_tcg::CodeStamp> {
            self.ram.code_stamp(address, len)
        }
        fn load_reserved(&mut self, address: u64, bytes: &mut [u8]) -> vmlab_hv_tcg::Result<u64> {
            self.ram.load_reserved(address, bytes)
        }
        fn store_conditional(
            &mut self,
            address: u64,
            bytes: &[u8],
            generation: u64,
        ) -> vmlab_hv_tcg::Result<bool> {
            self.ram.store_conditional(address, bytes, generation)
        }
        fn transaction_lock(&self) -> Option<std::sync::Arc<parking_lot::ReentrantMutex<()>>> {
            None
        }
    }
    for train in [false, true] {
        let mut ram = FlatMemory::new(0x8000_0000, vec![0; 8192]);
        ram.write(0x8000_0000, &0x00118193u32.to_le_bytes())
            .unwrap();
        ram.write(0x8000_0004, &0x0020b023u32.to_le_bytes())
            .unwrap();
        let mut cpu = Riscv64::new(FailingStore {
            ram,
            fail: false,
            stores: 0,
        })
        .unwrap();
        cpu.specialize = true;
        cpu.set_pc(0x8000_0000);
        cpu.set_gpr(1, 0x8000_1000).unwrap();
        cpu.set_gpr(2, 13).unwrap();
        if train {
            cpu.run(2).unwrap();
            cpu.set_pc(0x8000_0000);
            cpu.set_gpr(3, 0).unwrap();
        }
        let before = cpu.steps;
        cpu.interpreter.memory.fail = true;
        cpu.interpreter.memory.stores = 0;
        let error = cpu.run(2).unwrap_err().to_string();
        assert!(error.contains("physical access fault"), "{error}");
        assert_eq!(cpu.steps, before + 1);
        assert_eq!(cpu.gpr(3).unwrap(), 1);
        assert_eq!(cpu.interpreter.memory.stores, 1);
    }
}

#[test]
fn warmed_loop_does_not_start_promotions_at_the_run_tail() {
    let mut memory = FlatMemory::new(0x8000_0000, vec![0; 4096]);
    for (index, instruction) in [0x00108093u32, 0xfff10113, 0xfe011ce3].iter().enumerate() {
        memory
            .write(0x8000_0000 + index as u64 * 4, &instruction.to_le_bytes())
            .unwrap();
    }
    let mut cpu = Riscv64::new(memory).unwrap();
    cpu.specialize = true;
    cpu.set_pc(0x8000_0000);
    cpu.set_gpr(1, 1_000_000).unwrap();
    cpu.set_gpr(2, 1_000_000).unwrap();
    cpu.run(65_536).unwrap();
    cpu.set_pc(0x8000_0000);
    cpu.set_gpr(1, 1_000_000).unwrap();
    cpu.set_gpr(2, 1_000_000).unwrap();
    cpu.steps = 0;
    cpu.specialization_stats = Default::default();
    let operations = cpu.interpreter.operations;
    assert_eq!(cpu.run(8192).unwrap(), (8192, false));
    assert_eq!(cpu.interpreter.operations, operations);
    assert_eq!(cpu.specialization_stats.recordings, 0);
    assert_eq!(cpu.specialization_stats.exits, 0);
    assert!(cpu.specialization_stats.trace_runs <= 34);
    assert_eq!(cpu.gpr(1).unwrap(), 1_002_731);
    assert_eq!(cpu.gpr(2).unwrap(), 997_269);
    assert_eq!(cpu.pc().unwrap(), 0x8000_0008);
}

#[test]
fn data_registers_relax_a_failed_ram_range_without_repeated_recording() {
    let make = || {
        let mut memory = FlatMemory::new(0x8000_0000, vec![0; 4096]);
        memory
            .write(0x8000_0000, &0x00008193u32.to_le_bytes())
            .unwrap();
        Riscv64::new(memory).unwrap()
    };
    let mut reference = make();
    let mut specialized = make();
    specialized.specialize = true;
    for value in [0x8000_0008, 0, u64::MAX, 0x8000_0010, 123, 0x8000_0018, 456] {
        for cpu in [&mut reference, &mut specialized] {
            cpu.set_pc(0x8000_0000);
            cpu.set_gpr(1, value).unwrap();
        }
        assert_eq!(specialized.step().unwrap(), reference.step().unwrap());
        assert_eq!(
            *specialized.interpreter.globals,
            *reference.interpreter.globals
        );
        assert_eq!(specialized.gpr(3).unwrap(), value);
    }
    assert_eq!(specialized.specialization_stats.recordings, 2);
    assert_eq!(specialized.specialization_stats.evictions, 0);
}

#[test]
fn native_addresses_cross_ram_chunks_without_entry_respecialization() {
    let make = || {
        let mut memory = FlatMemory::new(0x8000_0000, vec![0; 0x20000]);
        memory
            .write(0x8000_0000, &0x0020b023u32.to_le_bytes())
            .unwrap();
        Riscv64::new(memory).unwrap()
    };
    let mut reference = make();
    let mut specialized = make();
    specialized.specialize = true;
    let mut trained = None;
    for (index, address) in [
        0x8000_1000,
        0x8000_1008,
        0x8001_1000,
        0x8001_1008,
        0x8000_1000,
        0x8001_1000,
    ]
    .into_iter()
    .enumerate()
    {
        for cpu in [&mut reference, &mut specialized] {
            cpu.set_pc(0x8000_0000);
            cpu.set_gpr(1, address).unwrap();
            cpu.set_gpr(2, address ^ 0x1234).unwrap();
        }
        assert_eq!(specialized.step().unwrap(), reference.step().unwrap());
        assert_eq!(
            *specialized.interpreter.globals,
            *reference.interpreter.globals
        );
        assert_eq!(
            specialized.interpreter.memory.bytes(),
            reference.interpreter.memory.bytes()
        );
        if index == 3 {
            trained = Some(specialized.specialization_stats.recordings);
        }
        if index > 3 {
            assert_eq!(Some(specialized.specialization_stats.recordings), trained);
        }
    }
    assert_eq!(specialized.specialization_stats.recordings, 1);
    assert_eq!(specialized.specialization_stats.misses, 0);
    assert!(specialized.specialization_stats.hits > 0);
}
