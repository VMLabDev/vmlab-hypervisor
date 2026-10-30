use vmlab_hv_lockstep::{CpuError, Memory, MemoryWrite, State, StepCpu, random, run};

struct Probe {
    state: State,
    memory: Memory,
    corrupt_at: Option<u64>,
    count: u64,
    corrupt_memory: bool,
}
impl Probe {
    fn new(corrupt_at: Option<u64>, corrupt_memory: bool) -> Self {
        Self {
            state: State::default(),
            memory: Memory::new(0, vec![]).unwrap(),
            corrupt_at,
            count: 0,
            corrupt_memory,
        }
    }
}
impl StepCpu for Probe {
    fn reset(&mut self, state: &State, memory: &Memory) -> Result<(), CpuError> {
        self.state = state.clone();
        self.memory = memory.clone();
        self.memory.take_writes();
        self.count = 0;
        if self.corrupt_at == Some(0) {
            self.state.pc += 4;
        }
        Ok(())
    }
    fn state(&mut self) -> Result<State, CpuError> {
        Ok(self.state.clone())
    }
    fn memory(&self) -> &Memory {
        &self.memory
    }
    fn step(&mut self) -> Result<Vec<MemoryWrite>, CpuError> {
        self.count += 1;
        self.state.pc += 4;
        if self.corrupt_at == Some(self.count) {
            if self.corrupt_memory {
                self.memory.write(self.memory.base + 64, &[7])?;
            } else {
                self.state.gpr[7] = 9;
                self.state.fpr[3] = u64::MAX;
                self.state.csrs.insert(0x300, 2);
                self.state.privilege = 1;
            }
        }
        Ok(self.memory.take_writes())
    }
}

#[test]
fn first_instruction_diff_has_original_pc_and_instruction() {
    let initial = State::default();
    let mut bytes = random::stream(42, 32);
    bytes.resize(256, 0);
    let memory = Memory::new(initial.pc, bytes).unwrap();
    let mut a = Probe::new(None, false);
    let mut b = Probe::new(Some(3), false);
    let error = run(&mut a, &mut b, &initial, &memory, 20).unwrap_err();
    let vmlab_hv_lockstep::RunError::Divergence(diff) = error else {
        panic!("wrong error")
    };
    assert_eq!(diff.step, 3);
    assert_eq!(diff.pc, initial.pc + 8);
    assert_eq!(diff.instruction, memory.read(initial.pc + 8, 4).unwrap());
    let message = diff.to_string();
    for name in ["x7", "f3", "csr 0x300", "privilege", "instruction="] {
        assert!(message.contains(name), "{message}");
    }
    assert_eq!(a.count, 3);
    assert_eq!(b.count, 3);
}

#[test]
fn detects_memory_only_divergence() {
    let initial = State::default();
    let memory = Memory::new(initial.pc, vec![0; 128]).unwrap();
    let error = run(
        &mut Probe::new(None, false),
        &mut Probe::new(Some(1), true),
        &initial,
        &memory,
        10,
    )
    .unwrap_err();
    assert!(error.to_string().contains("memory writes"));
}

#[test]
fn identical_cpus_and_memory_bounds() {
    let initial = State::default();
    let memory = Memory::new(initial.pc, vec![0; 128]).unwrap();
    run(
        &mut Probe::new(None, false),
        &mut Probe::new(None, false),
        &initial,
        &memory,
        10,
    )
    .unwrap();
    assert!(memory.read(u64::MAX, 8).is_err());
    assert!(memory.read(initial.pc - 1, 1).is_err());
    assert!(Memory::new(u64::MAX, vec![0]).is_err());
    assert!(memory.read(initial.pc, usize::MAX).is_err());
}

#[test]
fn random_encodings_are_seeded_and_rv64i() {
    let bytes = random::stream(91, 10000);
    assert_eq!(bytes, random::stream(91, 10000));
    assert_ne!(bytes, random::stream(92, 10000));
    for b in bytes.as_chunks::<4>().0 {
        let word = u32::from_le_bytes(*b);
        let f3 = word >> 12 & 7;
        match word & 0x7f {
            0x13 => match f3 {
                0 => {}
                1 => assert_eq!(word >> 26, 0),
                5 => assert!(matches!(word >> 26, 0 | 0x10)),
                _ => panic!("invalid OP-IMM"),
            },
            0x33 => assert_eq!(word >> 25, 0),
            0x17 | 0x37 => {}
            _ => panic!("invalid RV64I opcode"),
        }
    }
}

#[test]
fn malformed_elf_is_rejected_without_panics() {
    for size in 0..256 {
        let bytes = random::stream(size as u64, size);
        assert!(vmlab_hv_lockstep::corpus::TestImage::elf(&bytes, 0x80000000, 4096).is_err());
    }
}

#[test]
fn rejects_equal_but_incorrect_resets() {
    let initial = State::default();
    let memory = Memory::new(initial.pc, vec![0; 128]).unwrap();
    let error = run(
        &mut Probe::new(Some(0), false),
        &mut Probe::new(Some(0), false),
        &initial,
        &memory,
        0,
    )
    .unwrap_err();
    assert!(error.to_string().contains("left reset: pc"));
}
