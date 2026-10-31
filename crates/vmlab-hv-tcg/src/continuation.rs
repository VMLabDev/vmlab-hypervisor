use crate::{Error, Frame, Interpreter, Memory, Result, Value};
use std::sync::Arc;
use vmlab_hv_tcg_ir::{Function, Location};

pub(crate) struct RuntimeFrame {
    pub function: Arc<Function>,
    pub locals: Frame,
    pub pc: usize,
    /// The destination of an unfinished child call in this frame.
    pub pending_result: Option<Location>,
    pub call_site: usize,
}

impl<M: Memory> Interpreter<'_, M> {
    /// Resume a materialized root-to-leaf Sail stack without replaying its
    /// completed operations. The deepest frame runs first.
    pub(crate) fn resume(&mut self, frames: Vec<RuntimeFrame>) -> Result<Value> {
        self.resume_with_value(frames, None)
    }

    pub(crate) fn resume_with_value(
        &mut self,
        mut frames: Vec<RuntimeFrame>,
        mut returned: Option<Value>,
    ) -> Result<Value> {
        if frames.is_empty() {
            return Err(Error::Type("empty Sail continuation".into()));
        }
        let base_depth = self.depth;
        if frames.len() + base_depth as usize > 256 {
            return Err(Error::Limit);
        }
        let transaction = self.memory.transaction_lock();
        let _atomic = transaction.as_ref().map(|lock| lock.lock());
        let result = (|| {
            while let Some(mut frame) = frames.pop() {
                self.depth = base_depth + frames.len() as u32 + 1;
                let step = (|| {
                    if let Some(value) = returned.take() {
                        let location = frame.pending_result.as_ref().ok_or_else(|| {
                            Error::Type("continuation caller has no result destination".into())
                        })?;
                        if let Some(mut recorder) = self.recorder.take() {
                            recorder.resume_result(self, location);
                            self.recorder = Some(recorder);
                        }
                        self.assign(&mut frame.locals, location, value)
                            .map_err(|source| Error::At {
                                function: frame.function.name.clone(),
                                instruction: frame.call_site,
                                source: Box::new(source),
                            })?;
                    }
                    let result = self.run_frame_from(&frame.function, &mut frame.locals, frame.pc);
                    if let Some(mut recorder) = self.recorder.take() {
                        recorder.leave(self, result.is_ok());
                        self.recorder = Some(recorder);
                    }
                    result
                })();
                match step {
                    Ok(value) => returned = Some(value),
                    Err(mut error) => {
                        for caller in frames.iter().rev() {
                            error = Error::At {
                                function: caller.function.name.clone(),
                                instruction: caller.call_site,
                                source: Box::new(error),
                            };
                        }
                        return Err(error);
                    }
                }
            }
            returned.ok_or_else(|| Error::Type("continuation returned no value".into()))
        })();
        self.depth = base_depth;
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::FlatMemory;
    use std::collections::BTreeMap;
    use vmlab_hv_tcg_ir::{Expr, Op, Program, Target, Type};

    struct LoggedMemory {
        ram: FlatMemory,
        writes: Vec<(u64, Vec<u8>)>,
    }

    impl Memory for LoggedMemory {
        fn read(&mut self, address: u64, data: &mut [u8]) -> Result<()> {
            self.ram.read(address, data)
        }
        fn write(&mut self, address: u64, data: &[u8]) -> Result<()> {
            self.ram.write(address, data)?;
            self.writes.push((address, data.to_vec()));
            Ok(())
        }
        fn generation(&self) -> u64 {
            self.ram.generation()
        }
        fn load_reserved(&mut self, address: u64, data: &mut [u8]) -> Result<u64> {
            self.ram.load_reserved(address, data)
        }
        fn store_conditional(
            &mut self,
            address: u64,
            data: &[u8],
            generation: u64,
        ) -> Result<bool> {
            if self.generation() != generation {
                return Ok(false);
            }
            self.write(address, data)?;
            Ok(true)
        }
        fn transaction_lock(&self) -> Option<Arc<parking_lot::ReentrantMutex<()>>> {
            None
        }
    }

    fn store(address: u64, data: u8) -> Op {
        Op::Call(
            Location::Id("ok".into()),
            Target::Memory {
                name: "write_mem".into(),
                write: true,
                exclusive: false,
                fetch: false,
            },
            vec![
                Expr::Unit,
                Expr::Unit,
                Expr::Bits(format!("0x{address:016x}")),
                Expr::Integer("1".into(), None),
                Expr::Bits(format!("0x{data:02x}")),
            ],
        )
    }

    #[test]
    fn branch_exit_after_a_store_does_not_repeat_the_store() {
        let callee = Function {
            name: "callee".into(),
            args: vec![],
            result: Type::Integer(None),
            locals: BTreeMap::new(),
            code: vec![
                store(8, 42),
                Op::Branch(Expr::Id("decision".into()), 4),
                Op::Assign(
                    Location::Id("return".into()),
                    Expr::Integer("7".into(), None),
                ),
                Op::Jump(5),
                Op::Assign(
                    Location::Id("return".into()),
                    Expr::Integer("9".into(), None),
                ),
                Op::Return,
            ],
        };
        let caller = Function {
            name: "caller".into(),
            args: vec![],
            result: Type::Integer(None),
            locals: BTreeMap::new(),
            code: vec![
                Op::Declare("inner".into(), Type::Integer(None), None),
                Op::Call(
                    Location::Id("inner".into()),
                    Target::Function("callee".into()),
                    vec![],
                ),
                store(9, 153),
                Op::Assign(Location::Id("return".into()), Expr::Id("inner".into())),
                Op::Return,
            ],
        };
        let program = Program {
            functions: BTreeMap::from([
                ("callee".into(), callee.clone()),
                ("caller".into(), caller.clone()),
            ]),
            globals: BTreeMap::from([("decision".into(), Type::Bool)]),
            ..Program::default()
        };
        let memory = || LoggedMemory {
            ram: FlatMemory::new(0, vec![0; 16]),
            writes: vec![],
        };
        let mut reference = Interpreter::new(&program, memory());
        reference.initialize().unwrap();
        let expected = reference.call("caller", vec![]).unwrap();
        let mut resumed = Interpreter::new(&program, memory());
        resumed.initialize().unwrap();
        resumed.memory.write(8, &[42]).unwrap();
        let actual = resumed
            .resume(vec![
                RuntimeFrame {
                    function: Arc::new(caller),
                    locals: BTreeMap::from([
                        ("return".into(), Value::Undefined),
                        ("inner".into(), Value::Undefined),
                    ]),
                    pc: 2,
                    pending_result: Some(Location::Id("inner".into())),
                    call_site: 1,
                },
                RuntimeFrame {
                    function: Arc::new(callee),
                    locals: BTreeMap::from([
                        ("return".into(), Value::Undefined),
                        ("ok".into(), Value::Bool(true)),
                    ]),
                    pc: 2,
                    pending_result: None,
                    call_site: 1,
                },
            ])
            .unwrap();
        assert_eq!(actual, expected);
        assert_eq!(resumed.memory.writes, reference.memory.writes);
        assert_eq!(resumed.memory.writes, vec![(8, vec![42]), (9, vec![153])]);
        assert_eq!(resumed.depth, 0);
    }
}
