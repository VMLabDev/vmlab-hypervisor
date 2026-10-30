//! Adapter to the independent per-instruction comparison interface.
use crate::{Memory as PhysicalMemory, Riscv64, Value};
use vmlab_hv_lockstep::{CpuError, Memory, MemoryWrite, State, StepCpu};

pub struct LockstepMemory {
    memory: Memory,
    generation: u64,
    csrs: Vec<u16>,
}

impl LockstepMemory {
    fn new(mut memory: Memory, csrs: Vec<u16>) -> Self {
        memory.take_writes();
        Self {
            memory,
            generation: 0,
            csrs,
        }
    }
}

fn model_error(error: crate::Error) -> CpuError {
    CpuError(error.to_string())
}
fn memory_error(error: CpuError) -> crate::Error {
    crate::Error::Type(error.to_string())
}

impl PhysicalMemory for LockstepMemory {
    fn read(&mut self, address: u64, bytes: &mut [u8]) -> crate::Result<()> {
        bytes.copy_from_slice(
            self.memory
                .read(address, bytes.len())
                .map_err(memory_error)?,
        );
        Ok(())
    }
    fn write(&mut self, address: u64, bytes: &[u8]) -> crate::Result<()> {
        self.memory.write(address, bytes).map_err(memory_error)?;
        self.generation = self.generation.wrapping_add(1);
        Ok(())
    }
    fn generation(&self) -> u64 {
        self.generation
    }
    fn load_reserved(&mut self, address: u64, bytes: &mut [u8]) -> crate::Result<u64> {
        self.read(address, bytes)?;
        Ok(self.generation)
    }
    fn store_conditional(
        &mut self,
        address: u64,
        bytes: &[u8],
        generation: u64,
    ) -> crate::Result<bool> {
        if self.generation != generation {
            return Ok(false);
        }
        self.write(address, bytes)?;
        Ok(true)
    }
    fn transaction_lock(&self) -> Option<std::sync::Arc<parking_lot::ReentrantMutex<()>>> {
        None
    }
}

pub type LockstepCpu = Riscv64<LockstepMemory>;

impl LockstepCpu {
    pub fn for_lockstep(memory: Memory) -> Result<Self, CpuError> {
        Self::new(LockstepMemory::new(memory, Vec::new())).map_err(model_error)
    }
}

impl StepCpu for LockstepCpu {
    fn reset(&mut self, state: &State, memory: &Memory) -> Result<(), CpuError> {
        if state.gpr[0] != 0
            || !matches!(state.privilege, 0 | 1 | 3)
            || (state.privilege == 3 && state.virtualized)
        {
            return Err(CpuError("invalid architectural initial state".into()));
        }
        *self = Self::new(LockstepMemory::new(
            memory.clone(),
            state.csrs.keys().copied().collect(),
        ))
        .map_err(model_error)?;
        for (&csr, &value) in &state.csrs {
            self.write_csr(csr, value).map_err(model_error)?;
        }
        for i in 0..32 {
            self.set_gpr(i, state.gpr[usize::from(i)])
                .map_err(model_error)?;
            // Snapshot restoration must not mark FP state dirty.
            self.interpreter
                .globals
                .insert(format!("zf{i}"), Value::bits(64, state.fpr[usize::from(i)]));
        }
        self.set_pc(state.pc);
        let privilege = match (state.privilege, state.virtualized) {
            (3, false) => "zMachine",
            (1, false) => "zSupervisor",
            (0, false) => "zUser",
            (1, true) => "zVirtualSupervisor",
            (0, true) => "zVirtualUser",
            _ => return Err(CpuError("invalid privilege".into())),
        };
        self.interpreter
            .globals
            .insert("zcur_privilege".into(), Value::Enum(privilege.into()));
        self.interpreter.memory.memory.take_writes();
        Ok(())
    }
    fn state(&mut self) -> Result<State, CpuError> {
        // Observation executes generated CSR accessors too. Budget each bounded
        // snapshot independently instead of charging sampling to guest lifetime.
        self.interpreter.operation_limit = self.interpreter.operations.saturating_add(1_000_000);
        let mut state = State {
            pc: self.pc().map_err(model_error)?,
            ..State::default()
        };
        for i in 0..32 {
            state.gpr[i] = self.gpr(i as u8).map_err(model_error)?;
            state.fpr[i] = self.interpreter.globals[&format!("zf{i}")]
                .u64()
                .map_err(model_error)?;
        }
        let Value::Enum(privilege) = &self.interpreter.globals["zcur_privilege"] else {
            return Err(CpuError("invalid privilege value".into()));
        };
        (state.privilege, state.virtualized) = match privilege.as_str() {
            "zMachine" => (3, false),
            "zSupervisor" => (1, false),
            "zUser" => (0, false),
            "zVirtualSupervisor" => (1, true),
            "zVirtualUser" => (0, true),
            _ => return Err(CpuError("invalid privilege enum".into())),
        };
        for csr in self.interpreter.memory.csrs.clone() {
            state
                .csrs
                .insert(csr, self.read_csr(csr).map_err(model_error)?);
        }
        Ok(state)
    }
    fn memory(&self) -> &Memory {
        &self.interpreter.memory.memory
    }
    fn step(&mut self) -> Result<Vec<MemoryWrite>, CpuError> {
        self.interpreter.operation_limit = self.interpreter.operations.saturating_add(1_000_000);
        Riscv64::step(self).map_err(model_error)?;
        Ok(self.interpreter.memory.memory.take_writes())
    }
}
