use crate::{Error, Interpreter, Memory, Result, Value};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ExceptionLevel {
    El0,
    El1,
}

/// A64 execution through the generated Sail fetch, decode, and exception paths.
pub struct Aarch64<M> {
    pub interpreter: Interpreter<'static, M>,
    pub steps: u64,
}

impl<M: Memory> Aarch64<M> {
    pub fn new(memory: M) -> Result<Self> {
        let mut interpreter = Interpreter::new(vmlab_hv_tcg_aarch64::model(), memory);
        interpreter.initialize()?;
        interpreter.call("zinitializze_registers", vec![Value::Unit])?;
        interpreter.call("z__InitSystem", vec![Value::Unit])?;
        let mut cpu = Self {
            interpreter,
            steps: 0,
        };
        cpu.write_system_register("SCR_EL3", 0x501)?;
        cpu.write_system_register("HCR_EL2", 1 << 31)?;
        cpu.write_system_register("CPTR_EL3", 0)?;
        cpu.write_system_register("CPTR_EL2", 0)?;
        cpu.write_system_register("CPACR_EL1", 3 << 20)?;
        cpu.write_system_register("SCTLR_EL1", 0x30d0_0800)?;
        cpu.set_exception_level(ExceptionLevel::El1)?;
        cpu.check_exception()?;
        Ok(cpu)
    }

    pub fn pc(&self) -> Result<u64> {
        self.interpreter.globals["z_PC"].u64()
    }

    pub fn set_pc(&mut self, pc: u64) {
        self.interpreter
            .globals
            .insert("z_PC".into(), Value::bits(64, pc));
    }

    pub fn gpr(&self, register: u8) -> Result<u64> {
        match register {
            0..=30 => self.interpreter.globals[&format!("zR{register}")].u64(),
            31 => Ok(0),
            _ => Err(Error::Type("GPR index outside 0..32".into())),
        }
    }

    pub fn set_gpr(&mut self, register: u8, value: u64) -> Result<()> {
        match register {
            0..=30 => {
                self.interpreter
                    .globals
                    .insert(format!("zR{register}"), Value::bits(64, value));
                Ok(())
            }
            31 => Ok(()),
            _ => Err(Error::Type("GPR index outside 0..32".into())),
        }
    }

    pub fn set_exception_level(&mut self, level: ExceptionLevel) -> Result<()> {
        let Value::Struct(fields) = self
            .interpreter
            .globals
            .get_mut("zPSTATE")
            .ok_or_else(|| Error::Undefined("PSTATE".into()))?
        else {
            return Err(Error::Type("PSTATE is not a struct".into()));
        };
        for name in ["zD", "zA", "zI", "zF"] {
            fields.insert(name.into(), Value::bits(1, 1u8));
        }
        let el = u8::from(level == ExceptionLevel::El1);
        fields.insert("zEL".into(), Value::bits(2, el));
        fields.insert("zSP".into(), Value::bits(1, el));
        fields.insert("znRW".into(), Value::bits(1, 0u8));
        Ok(())
    }

    pub fn read_system_register(&self, name: &str) -> Result<u64> {
        let value = self
            .interpreter
            .globals
            .get(&format!("z{name}"))
            .ok_or_else(|| Error::Undefined(format!("system register {name}")))?;
        match value {
            Value::Bits { .. } => value.u64(),
            Value::Struct(fields) if fields.len() == 1 => value.field("zbits")?.u64(),
            _ => Err(Error::Type(format!("system register {name} is not scalar"))),
        }
    }

    pub fn write_system_register(&mut self, name: &str, value: u64) -> Result<()> {
        let register = self
            .interpreter
            .globals
            .get_mut(&format!("z{name}"))
            .ok_or_else(|| Error::Undefined(format!("system register {name}")))?;
        let slot = match register {
            Value::Struct(fields) if fields.len() == 1 => fields
                .get_mut("zbits")
                .ok_or_else(|| Error::Type(format!("system register {name} has no bits")))?,
            value => value,
        };
        let Value::Bits { width, .. } = slot else {
            return Err(Error::Type(format!("system register {name} is not scalar")));
        };
        *slot = Value::bits(*width, value);
        Ok(())
    }

    pub fn step(&mut self) -> Result<()> {
        self.interpreter.operation_limit = self.interpreter.operations.saturating_add(100_000_000);
        self.interpreter
            .call("z__InstructionExecute", vec![Value::Unit])?;
        self.check_exception()?;
        self.interpreter.call("z__CycleEnd", vec![Value::Unit])?;
        self.check_exception()?;
        self.steps += 1;
        Ok(())
    }

    fn check_exception(&self) -> Result<()> {
        if self.interpreter.globals["have_exception"].boolean()? {
            return Err(Error::Exception(format!(
                "uncaught AArch64 model exception: {:?}",
                self.interpreter.globals["current_exception"]
            )));
        }
        Ok(())
    }
}
