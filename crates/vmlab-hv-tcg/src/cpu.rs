use crate::{Interpreter, Memory, Result, Value};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Privilege {
    User,
    Supervisor,
    Machine,
}

/// One hart owns its architectural state and reservation. RAM sharing belongs
/// to the Memory implementation; no CPU state is stored in process globals.
pub struct Riscv64<M> {
    pub interpreter: Interpreter<'static, M>,
    pub steps: u64,
}

impl<M: Memory> Riscv64<M> {
    pub fn new(memory: M) -> Result<Self> {
        let mut interpreter = Interpreter::new(vmlab_hv_tcg_riscv64::model(), memory);
        interpreter.initialize()?;
        interpreter.call("zinitializze_registers", vec![Value::Unit])?;
        interpreter.call("zreset", vec![Value::Unit])?;
        Ok(Self {
            interpreter,
            steps: 0,
        })
    }
    pub fn pc(&self) -> Result<u64> {
        self.interpreter.globals["zPC"].u64()
    }
    pub fn set_pc(&mut self, pc: u64) {
        self.interpreter
            .globals
            .insert("zPC".into(), Value::bits(64, pc));
        self.interpreter
            .globals
            .insert("znextPC".into(), Value::bits(64, pc));
    }
    pub fn gpr(&self, register: u8) -> Result<u64> {
        if register == 0 {
            return Ok(0);
        }
        self.interpreter
            .globals
            .get(&format!("zx{register}"))
            .ok_or_else(|| crate::Error::Type("GPR index outside 0..32".into()))?
            .u64()
    }
    pub fn set_gpr(&mut self, register: u8, value: u64) -> Result<()> {
        if register >= 32 {
            return Err(crate::Error::Type("GPR index outside 0..32".into()));
        }
        if register != 0 {
            self.interpreter
                .globals
                .insert(format!("zx{register}"), Value::bits(64, value));
        }
        Ok(())
    }
    pub fn set_privilege(&mut self, privilege: Privilege) {
        self.interpreter.globals.insert(
            "zcur_privilege".into(),
            Value::Enum(
                match privilege {
                    Privilege::User => "zUser",
                    Privilege::Supervisor => "zSupervisor",
                    Privilege::Machine => "zMachine",
                }
                .into(),
            ),
        );
    }
    pub fn read_csr(&mut self, csr: u16) -> Result<u64> {
        self.interpreter
            .call("zread_CSR", vec![Value::bits(12, csr)])?
            .u64()
    }
    pub fn write_csr(&mut self, csr: u16, value: u64) -> Result<()> {
        self.interpreter.call(
            "zwrite_CSR",
            vec![Value::bits(12, csr), Value::bits(64, value)],
        )?;
        Ok(())
    }
    /// Executes fetch, translation, decode, semantics and trap entry from Sail.
    /// Returns true if the hart is waiting rather than advancing its PC.
    pub fn step(&mut self) -> Result<bool> {
        self.interpreter.fetch_address = None;
        let waiting = self
            .interpreter
            .call(
                "ztry_step",
                vec![Value::int(self.steps), Value::Bool(false)],
            )?
            .boolean()?;
        self.interpreter.call("ztick_clock", vec![Value::Unit])?;
        self.steps += 1;
        Ok(waiting)
    }
}
