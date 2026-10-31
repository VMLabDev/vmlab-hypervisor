#![cfg(feature = "lockstep")]
use vmlab_hv_lockstep::{Memory, State, StepCpu};
use vmlab_hv_tcg::lockstep::LockstepCpu;

#[test]
fn adapter_restores_state_clears_writes_and_resets_counters() {
    let mut bytes = vec![0; 4096];
    bytes[0..4].copy_from_slice(&0x00113023u32.to_le_bytes()); // sd x1,0(x2)
    let memory = Memory::new(0x8000_0000, bytes).unwrap();
    let mut state = State::default();
    state.gpr[1] = 7;
    state.gpr[2] = 0x8000_0800;
    state.fpr[3] = 0x7ff8_0123_4567_89ab;
    state.csrs.insert(0xb00, 0);
    let mut cpu = LockstepCpu::for_lockstep(memory.clone()).unwrap();
    cpu.reset(&state, &memory).unwrap();
    assert_eq!(StepCpu::state(&mut cpu).unwrap(), state);
    let writes = StepCpu::step(&mut cpu).unwrap();
    assert_eq!(writes.len(), 1);
    assert_eq!(writes[0].address, state.gpr[2]);
    assert_eq!(writes[0].bytes, 7u64.to_le_bytes());
    assert_eq!(StepCpu::state(&mut cpu).unwrap().csrs[&0xb00], 1);
    cpu.reset(&state, &memory).unwrap();
    assert_eq!(StepCpu::state(&mut cpu).unwrap(), state);
    assert_eq!(StepCpu::memory(&cpu).bytes(), memory.bytes());
    assert_eq!(StepCpu::step(&mut cpu).unwrap().len(), 1);
}
