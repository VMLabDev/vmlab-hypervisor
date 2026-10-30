#![cfg(feature = "sail")]
use std::path::PathBuf;
use vmlab_hv_lockstep::{Memory, State, StepCpu, random, run, sail::Sail};

static ORACLE_TESTS: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn paths() -> (PathBuf, PathBuf) {
    (
        std::env::var_os("VMLAB_SAIL_ORACLE")
            .expect("set VMLAB_SAIL_ORACLE to the built adapter .so")
            .into(),
        std::env::var_os("VMLAB_SAIL_CONFIG")
            .expect("set VMLAB_SAIL_CONFIG to rv64d_v128_e64.json")
            .into(),
    )
}

#[test]
fn oracle_against_itself_random_and_reset() {
    let _guard = ORACLE_TESTS.lock().unwrap();
    let (library, config) = paths();
    let bytes = random::stream(0x12345678, 4096);
    let memory = Memory::new(0x80000000, bytes).unwrap();
    let mut left = Sail::open(&library, &config, memory.clone()).unwrap();
    let mut right = Sail::open(&library, &config, memory.clone()).unwrap();
    let initial = left.state().unwrap();
    run(&mut left, &mut right, &initial, &memory, 4096).unwrap();
    assert_eq!(left.state().unwrap().pc, initial.pc + 4096 * 4);
    run(&mut left, &mut right, &initial, &memory, 16).unwrap();
    assert_eq!(left.state().unwrap().pc, initial.pc + 64);
}

#[test]
fn oracle_executes_integer_store_load_and_branch() {
    let _guard = ORACLE_TESTS.lock().unwrap();
    let (library, config) = paths();
    // addi x1,x0,7; sd x1,0(x2); ld x3,0(x2); beq x1,x3,+8; addi x1,x0,0; addi x4,x0,9
    let words: [u32; 6] = [
        0x00700093, 0x00113023, 0x00013183, 0x00308463, 0x00000093, 0x00900213,
    ];
    let mut bytes: Vec<u8> = words.iter().flat_map(|w| w.to_le_bytes()).collect();
    bytes.resize(4096, 0);
    let memory = Memory::new(0x80000000, bytes).unwrap();
    let mut a = Sail::open(&library, &config, memory.clone()).unwrap();
    let mut b = Sail::open(&library, &config, memory.clone()).unwrap();
    let mut initial = a.state().unwrap();
    initial.gpr[2] = 0x80000800;
    run(&mut a, &mut b, &initial, &memory, 5).unwrap();
    let state = a.state().unwrap();
    assert_eq!(state.gpr[1], 7);
    assert_eq!(state.gpr[3], 7);
    assert_eq!(state.gpr[4], 9);
    assert_eq!(
        a.memory().read(initial.gpr[2], 8).unwrap(),
        7u64.to_le_bytes()
    );
    assert_eq!(state.pc, initial.pc + 24);
}

#[test]
fn oracle_reports_unmapped_memory_as_error() {
    let _guard = ORACLE_TESTS.lock().unwrap();
    let (library, config) = paths();
    let memory = Memory::new(0x80000000, 0x00013083u32.to_le_bytes().to_vec()).unwrap(); // ld x1,0(x2)
    let mut a = Sail::open(&library, &config, memory.clone()).unwrap();
    let mut initial = State {
        pc: memory.base,
        ..State::default()
    };
    initial.gpr[2] = 0x80001000;
    a.reset(&initial, &memory).unwrap();
    assert!(a.step().is_err());
}

#[test]
fn oracle_restores_fp_status_and_executes_softfloat() {
    let _guard = ORACLE_TESTS.lock().unwrap();
    let (library, config) = paths();
    let memory = Memory::new(0x80000000, 0x022081d3u32.to_le_bytes().to_vec()).unwrap();
    let mut a = Sail::open(&library, &config, memory.clone()).unwrap();
    let mut b = Sail::open(&library, &config, memory.clone()).unwrap();
    let mut initial = a.state().unwrap();
    let clean = initial.clone();
    a.reset(&initial, &memory).unwrap();
    assert_eq!(
        a.state().unwrap(),
        clean,
        "restoring FPRs must not dirty FS"
    );
    initial.fpr[1] = 1.5f64.to_bits();
    initial.fpr[2] = 2.25f64.to_bits();
    for csr in [0x100, 0x300] {
        *initial.csrs.get_mut(&csr).unwrap() |= 0x6000 | (1 << 63);
    }
    run(&mut a, &mut b, &initial, &memory, 1).unwrap();
    assert_eq!(a.state().unwrap().fpr[3], 3.75f64.to_bits());
}

#[test]
fn oracle_steps_compressed_instructions() {
    let _guard = ORACLE_TESTS.lock().unwrap();
    let (library, config) = paths();
    let memory = Memory::new(0x80000000, vec![0x85, 0, 0x85, 0]).unwrap();
    let mut a = Sail::open(&library, &config, memory.clone()).unwrap();
    let mut b = Sail::open(&library, &config, memory.clone()).unwrap();
    let initial = a.state().unwrap();
    run(&mut a, &mut b, &initial, &memory, 2).unwrap();
    assert_eq!(a.state().unwrap().gpr[1], 2);
    assert_eq!(a.state().unwrap().pc, initial.pc + 4);
}
