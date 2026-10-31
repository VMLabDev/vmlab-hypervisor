use crate::{Error, Frame, Interpreter, Memory, Result, Value};
use std::collections::BTreeMap;
use std::sync::Arc;
use vmlab_hv_tcg_ir::indexed::{IndexedFunction, IndexedOp, Operand, Place};
use vmlab_hv_tcg_ir::{Function, Target, Type};

pub(crate) struct Compiled {
    function: IndexedFunction,
    constants: Vec<Value>,
}

struct Slots {
    values: Vec<Option<Value>>,
    extra: BTreeMap<String, Value>,
}

impl<M: Memory> Interpreter<'_, M> {
    pub(crate) fn run_cached(&mut self, function: &Function, args: Vec<Value>) -> Result<Value> {
        if !self.indexed || self.recorder.is_some() {
            return self.run_function(function, args);
        }
        if self.depth >= 256 {
            return Err(Error::Limit);
        }
        if args.len() != function.args.len() {
            return Err(Error::Type(format!("{} argument count", function.name)));
        }
        let compiled = if let Some(compiled) = self.indexed_cache.get(&function.name) {
            compiled.clone()
        } else {
            let indexed = vmlab_hv_tcg_ir::indexed::index(function);
            let constants = indexed
                .constants
                .iter()
                .map(|expr| self.eval(&Frame::new(), expr))
                .collect::<Result<_>>()?;
            let compiled = Arc::new(Compiled {
                function: indexed,
                constants,
            });
            self.indexed_cache
                .insert(function.name.clone(), compiled.clone());
            compiled
        };
        let indexed = &compiled.function;
        let mut slots = Slots {
            values: vec![None; indexed.names.len()],
            extra: BTreeMap::new(),
        };
        for (slot, value) in indexed.arguments.iter().zip(args) {
            slots.values[*slot] = Some(value);
        }
        slots.values[indexed.result] = Some(
            if matches!(function.result, Type::Struct(_) | Type::List(_)) {
                self.default_value(&function.result, 0)?
            } else {
                Value::Undefined
            },
        );
        self.depth += 1;
        let start = self.operations;
        if let Some(profile) = &mut self.profile {
            profile.children.push(0);
        }
        let result = self.run_slots(&compiled, &mut slots, &function.name);
        if let Some(profile) = &mut self.profile {
            let inclusive = self.operations - start;
            let children = profile.children.pop().expect("profile frame");
            let counts = profile.functions.entry(function.name.clone()).or_default();
            counts.0 += 1;
            counts.1 += inclusive - children;
            counts.2 += inclusive;
            if let Some(parent) = profile.children.last_mut() {
                *parent += inclusive;
            }
        }
        self.depth -= 1;
        result
    }

    fn borrowed<'b>(
        &'b self,
        compiled: &'b Compiled,
        slots: &'b Slots,
        operand: &Operand,
    ) -> Result<Option<&'b Value>> {
        Ok(match operand {
            Operand::Local(slot) => Some(
                slots.values[*slot]
                    .as_ref()
                    .or_else(|| slots.extra.get(&compiled.function.names[*slot]))
                    .or_else(|| self.globals.get(&compiled.function.names[*slot]))
                    .ok_or_else(|| Error::Undefined(compiled.function.names[*slot].clone()))?,
            ),
            Operand::Global(name) => Some(
                slots
                    .extra
                    .get(name)
                    .or_else(|| self.globals.get(name))
                    .ok_or_else(|| Error::Undefined(name.clone()))?,
            ),
            Operand::Constant(slot) => Some(&compiled.constants[*slot]),
            Operand::Field(base, field) => match self.borrowed(compiled, slots, base)? {
                Some(value) => Some(value.field(field)?),
                None => None,
            },
            Operand::As(base, tag) => match self.borrowed(compiled, slots, base)? {
                Some(Value::Union(actual, value)) if actual == tag => Some(value),
                Some(_) => return Err(Error::Type(format!("expected constructor {tag}"))),
                None => None,
            },
            _ => None,
        })
    }

    fn operand(&mut self, compiled: &Compiled, slots: &Slots, operand: &Operand) -> Result<Value> {
        if let Some(value) = self.borrowed(compiled, slots, operand)? {
            return Ok(value.clone());
        }
        Ok(match operand {
            Operand::Ref(name) => Value::Ref(name.clone()),
            Operand::Struct(_, fields) => Value::Struct(
                fields
                    .iter()
                    .map(|(name, expr)| Ok((name.clone(), self.operand(compiled, slots, expr)?)))
                    .collect::<Result<_>>()?,
            ),
            Operand::Field(base, field) => {
                self.operand(compiled, slots, base)?.field(field)?.clone()
            }
            Operand::Is(base, tag) => {
                if let Some(value) = self.borrowed(compiled, slots, base)? {
                    Value::Bool(!matches!(value, Value::Union(actual, _) if actual == tag))
                } else {
                    Value::Bool(
                        !matches!(self.operand(compiled, slots, base)?, Value::Union(ref actual, _) if actual == tag),
                    )
                }
            }
            Operand::As(base, tag) => match self.operand(compiled, slots, base)? {
                Value::Union(actual, value) if actual == *tag => *value,
                _ => return Err(Error::Type(format!("expected constructor {tag}"))),
            },
            Operand::Intrinsic(name, width, operands) => {
                let args = self.operands(compiled, slots, operands)?;
                self.intrinsic(name, *width, &args)?
            }
            _ => return Err(Error::Type("unresolved indexed operand".into())),
        })
    }

    fn operands(
        &mut self,
        compiled: &Compiled,
        slots: &Slots,
        operands: &[Operand],
    ) -> Result<Vec<Value>> {
        operands
            .iter()
            .map(|operand| self.operand(compiled, slots, operand))
            .collect()
    }

    fn assign_slot(
        &mut self,
        compiled: &Compiled,
        slots: &mut Slots,
        place: &Place,
        value: Value,
    ) -> Result<()> {
        fn path<'b>(place: &'b Place, fields: &mut Vec<&'b str>) -> &'b Place {
            if let Place::Field(base, field) = place {
                let root = path(base, fields);
                fields.push(field);
                root
            } else {
                place
            }
        }
        let mut fields = Vec::new();
        let root = path(place, &mut fields);
        if let Place::Deref(base) = root {
            if !fields.is_empty() {
                return Err(Error::Type("field through reference".into()));
            }
            let operand = match base.as_ref() {
                Place::Local(slot) => Operand::Local(*slot),
                Place::Global(name) => Operand::Global(name.clone()),
                _ => return Err(Error::Type("nested dereference".into())),
            };
            let Value::Ref(name) = self.operand(compiled, slots, &operand)? else {
                return Err(Error::Type("expected reference".into()));
            };
            let target = compiled
                .function
                .slots
                .get(&name)
                .map_or_else(|| Place::Global(name.clone()), |slot| Place::Local(*slot));
            return self.assign_slot(compiled, slots, &target, value);
        }
        let name = match root {
            Place::Local(slot) => &compiled.function.names[*slot],
            Place::Global(name) => name,
            _ => return Err(Error::Type("indexed assignment root".into())),
        };
        let local = match root {
            Place::Local(slot) => Some(*slot),
            _ => None,
        };
        let is_local = local.is_some_and(|slot| slots.values[slot].is_some());
        if fields.is_empty() {
            if is_local || !self.globals.contains_key(name) {
                if let Some(slot) = local {
                    slots.values[slot] = Some(value);
                } else {
                    slots.extra.insert(name.clone(), value);
                }
            } else {
                self.globals.insert(name.clone(), value);
            }
            return Ok(());
        }
        let mut slot = if is_local || !self.globals.contains_key(name) {
            if let Some(index) = local {
                slots.values[index].get_or_insert(Value::Undefined)
            } else {
                slots.extra.entry(name.clone()).or_insert(Value::Undefined)
            }
        } else {
            self.globals.get_mut(name).expect("global exists")
        };
        for field in fields {
            let Value::Struct(map) = slot else {
                return Err(Error::Type("assignment to non-struct field".into()));
            };
            slot = map
                .get_mut(field)
                .ok_or_else(|| Error::Type(format!("missing field {field}")))?;
        }
        *slot = value;
        Ok(())
    }

    fn run_slots(&mut self, compiled: &Compiled, slots: &mut Slots, name: &str) -> Result<Value> {
        let mut pc = 0;
        while let Some(op) = compiled.function.code.get(pc) {
            self.operations += 1;
            if self.operations > self.operation_limit {
                return Err(Error::Limit);
            }
            let step = (|| -> Result<Option<usize>> {
                match op {
                    IndexedOp::Declare(slot, ty, expression) => {
                        slots.values[*slot] = Some(match expression {
                            Some(expr) => self.operand(compiled, slots, expr)?,
                            None if matches!(ty, Type::List(_) | Type::Struct(_)) => {
                                self.default_value(ty, 0)?
                            }
                            None => Value::Undefined,
                        });
                    }
                    IndexedOp::Assign(place, expression) => {
                        let value = self.operand(compiled, slots, expression)?;
                        self.assign_slot(compiled, slots, place, value)?;
                    }
                    IndexedOp::Call(place, target, operands) => {
                        let args = self.operands(compiled, slots, operands)?;
                        let value = match target {
                            Target::Function(name) => self.call(name, args)?,
                            Target::Intrinsic(name) => self.intrinsic(name, None, &args)?,
                            Target::Native { operation, width } => {
                                self.native(operation, *width, &args)?
                            }
                            Target::Constructor(name) => {
                                if args.len() != 1 {
                                    return Err(Error::Type("constructor arity".into()));
                                }
                                Value::Union(name.clone(), Box::new(args[0].clone()))
                            }
                            Target::Memory {
                                write,
                                exclusive,
                                fetch,
                                ..
                            } => {
                                if *fetch && self.fetch_address.is_none() {
                                    self.fetch_address = Some(args[2].u64()?);
                                }
                                self.memory_operation(&args, *write, *exclusive)?
                            }
                        };
                        self.assign_slot(compiled, slots, place, value)?;
                    }
                    IndexedOp::Branch(operand, target) => {
                        if self.operand(compiled, slots, operand)?.boolean()? {
                            return Ok(Some(*target));
                        }
                    }
                    IndexedOp::Jump(target) => return Ok(Some(*target)),
                    IndexedOp::Return => return Ok(None),
                    IndexedOp::Raise(cause) => return Err(Error::Exception(cause.clone())),
                    IndexedOp::Specialize => {}
                }
                Ok(Some(pc + 1))
            })();
            match step {
                Ok(Some(next)) => pc = next,
                Ok(None) => {
                    return Ok(slots.values[compiled.function.result]
                        .take()
                        .unwrap_or(Value::Unit));
                }
                Err(source) => {
                    return Err(Error::At {
                        function: name.into(),
                        instruction: pc,
                        source: Box::new(source),
                    });
                }
            }
        }
        Err(Error::Type(format!("{name} fell off IR")))
    }
}
