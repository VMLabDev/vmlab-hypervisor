use crate::continuation::RuntimeFrame;
use crate::record::{Input, InputSource, Options, Projection, Recorded, Recorder, Shape};
use crate::{Error, Frame, Interpreter, Memory, Result, Value};
use num_traits::ToPrimitive;
use std::cell::{Cell, RefCell};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use vmlab_hv_tcg_ir::Function;
use vmlab_hv_tcg_ir::native::block::{Binding, DeferredOperation, Event, Recipe, ValueId};
use vmlab_hv_tcg_ir::native::{
    Graph, Operation, ScalarId, ScalarType, evaluate_binary, evaluate_unary, mask,
};

type Stage = Vec<vmlab_hv_tcg_ir::native::packed::Packed>;

fn lower_stage(graph: &Graph, slots: &[usize], stage: Vec<ScalarId>) -> Stage {
    stage
        .into_iter()
        .filter_map(|id| {
            if matches!(
                graph.scalars[id].operation,
                Operation::Constant(_) | Operation::Refine(_) | Operation::Input(_)
            ) {
                return None;
            }
            let mut instruction = vmlab_hv_tcg_ir::native::linear::lower(graph, id)?;
            instruction.remap(slots);
            Some(
                vmlab_hv_tcg_ir::native::packed::Packed::new(slots[id], instruction)
                    .expect("bounded physical scalar instruction"),
            )
        })
        .collect()
}

pub(crate) struct Trace {
    recorded: Recorded,
    revision: Option<u64>,
    context_slots: Vec<usize>,
    stages: Vec<Stage>,
    checkpoint_stages: Vec<Stage>,
    fused_guards: Vec<Option<vmlab_hv_tcg_ir::native::linear::Guard>>,
    scratch: Vec<u128>,
    stamps: Vec<u64>,
    epoch: u64,
    waiting: Vec<Option<bool>>,
    global_identity: u64,
    input_values: Vec<bool>,
    successful_runs: u32,
    trained_steps: u32,
    cache_units: usize,
    pub last_used: u64,
    options: Options,
    fragments: RefCell<BTreeMap<usize, Vec<Trace>>>,
    fragment_units: Cell<usize>,
    branch_depth: u8,
    input_slots: Vec<Option<usize>>,
    binding_slots: HashMap<usize, Vec<usize>>,
    pc_slot: Option<usize>,
    compact_loop: std::sync::OnceLock<bool>,
    wide_inputs: Vec<Option<Value>>,
    scalar_input_slots: Vec<Option<usize>>,
    slots: Vec<usize>,
    fast_events: Vec<usize>,
    fast_code: Stage,
    event_steps: Vec<u32>,
    waiting_is_false: bool,
}

pub(crate) enum Outcome {
    Miss,
    Returned(Value, u32),
    Resumed(Value, u32),
}

struct Evaluation<'a> {
    recorded: &'a Recorded,
    wide_inputs: Vec<Option<Value>>,
    scalar_input_slots: &'a [Option<usize>],
    scalars: Vec<u128>,
    stamps: Vec<u64>,
    epoch: u64,
    completed_steps: u32,
    binding_slots: &'a HashMap<usize, Vec<usize>>,
    slots: &'a [usize],
}

enum Captured {
    Word(ScalarType, u128),
    Value(Value),
    Verified,
}

impl Captured {
    fn value(&self) -> Result<Value> {
        Ok(match self {
            Self::Word(ty, value) => match ty {
                ScalarType::Bool => Value::Bool(*value != 0),
                ScalarType::Bits(width) => Value::bits((*width).into(), *value),
                ScalarType::Integer => Value::int(*value as i128),
            },
            Self::Value(value) => value.clone(),
            Self::Verified => return Err(Error::Type("shape-only input used as a value".into())),
        })
    }
}

fn raw(value: &Value, ty: ScalarType) -> Option<u128> {
    match (value, ty) {
        (Value::Bool(value), ScalarType::Bool) => Some(u128::from(*value)),
        (Value::Bits { width, value }, ScalarType::Bits(w)) if *width == u32::from(w) => {
            value.to_u128().filter(|value| *value <= mask(w))
        }
        (Value::Integer(value), ScalarType::Integer) => value.to_i128().map(|v| v as u128),
        _ => None,
    }
}

fn capture_value(descriptor: &Input, value: &Value, full: bool) -> Option<Captured> {
    let valid = match (&descriptor.shape, value) {
        (Shape::Scalar(ty), value) => raw(value, *ty).is_some_and(|n| {
            descriptor
                .range
                .is_none_or(|range| range.contains(n as i128))
        }),
        (Shape::Bits(w), Value::Bits { width, .. }) => w == width,
        (Shape::Struct(keys), Value::Struct(fields)) => keys.iter().eq(fields.keys()),
        (Shape::Vector(len), Value::Vector(values)) => *len == values.len(),
        (Shape::Union(tag), Value::Union(actual, _)) => tag == actual,
        (Shape::Other(expected), value) => expected == value,
        _ => false,
    };
    if !valid {
        return None;
    }
    Some(if let Shape::Scalar(ty) = descriptor.shape {
        Captured::Word(ty, raw(value, ty)?)
    } else if full {
        Captured::Value(value.clone())
    } else {
        Captured::Verified
    })
}

fn capture_global_word(descriptor: &Input, word: (ScalarType, u128)) -> Option<Captured> {
    let (ty, raw) = word;
    if !matches!(descriptor.shape, Shape::Scalar(expected) if expected == ty)
        || !descriptor
            .range
            .is_none_or(|range| range.contains(raw as i128))
    {
        return None;
    }
    Some(Captured::Word(ty, raw))
}

fn project_value<'a>(mut value: &'a Value, path: &[Projection]) -> Option<&'a Value> {
    for projection in path {
        value = match (projection, value) {
            (Projection::Field(name), Value::Struct(fields)) => fields.get(name)?,
            (Projection::Index(index), Value::Vector(values)) => values.get(*index)?,
            (Projection::Payload(tag), Value::Union(actual, payload)) if tag == actual => payload,
            _ => return None,
        };
    }
    Some(value)
}

fn input<'a>(
    source: &InputSource,
    interpreter: &'a Interpreter<'_, impl Memory>,
    args: &'a [Value],
) -> Option<&'a Value> {
    match source {
        InputSource::Argument(index) => args.get(*index),
        InputSource::Local(..) | InputSource::FetchAddress => None,
        InputSource::Global(name, path) => {
            let mut value = interpreter.globals.get(name)?;
            for projection in path {
                value = match (projection, value) {
                    (Projection::Field(name), Value::Struct(fields)) => fields.get(name)?,
                    (Projection::Index(index), Value::Vector(values)) => values.get(*index)?,
                    (Projection::Payload(tag), Value::Union(actual, payload)) if tag == actual => {
                        payload
                    }
                    _ => return None,
                };
            }
            Some(value)
        }
    }
}

impl Evaluation<'_> {
    #[inline(always)]
    fn linear(&mut self, instruction: &vmlab_hv_tcg_ir::native::packed::Packed) -> Result<()> {
        let value = instruction
            .evaluate(&self.scalars)
            .ok_or_else(|| Error::Type("native scalar domain".into()))?;
        let id = instruction.output as usize;
        self.scalars[id] = value;
        self.stamps[id] = self.epoch;
        Ok(())
    }
    fn scalar(&mut self, id: ScalarId) -> Result<u128> {
        let slot = self.slots[id];
        if self.stamps[slot] == self.epoch || self.stamps[slot] == u64::MAX {
            return Ok(self.scalars[slot]);
        }
        let node = &self.recorded.block.graph.scalars[id];
        let value = match node.operation {
            Operation::Refine(value) => self.scalar(value)?,
            Operation::Constant(value) => value,
            Operation::Input(_) => {
                return Err(Error::Type("native input was not bound at entry".into()));
            }
            Operation::Effect(_) => {
                return Err(Error::Type("native effect used before execution".into()));
            }
            Operation::Unary(op, value) => evaluate_unary(op, self.scalar(value)?)
                .ok_or_else(|| Error::Type("native unary domain".into()))?,
            Operation::Binary(op, a, b) => {
                evaluate_binary(op, node.ty, self.scalar(a)?, self.scalar(b)?)
                    .ok_or_else(|| Error::Type("native binary domain".into()))?
            }
            Operation::Slice {
                value,
                offset,
                width,
            } => self.scalar(value)?.checked_shr(offset.into()).unwrap_or(0) & mask(width),
            Operation::Concat {
                high,
                low,
                low_width,
            } => {
                self.scalar(high)?
                    .checked_shl(low_width.into())
                    .unwrap_or(0)
                    | self.scalar(low)?
            }
        };
        let value = if let ScalarType::Bits(width) = node.ty {
            value & mask(width)
        } else {
            value
        };
        self.stamps[slot] = self.epoch;
        self.scalars[slot] = value;
        Ok(value)
    }

    fn value<M: Memory>(
        &mut self,
        interpreter: &mut Interpreter<'_, M>,
        id: ValueId,
    ) -> Result<Value> {
        Ok(match self.recorded.block.values[id].clone() {
            Recipe::Constant(id) => self.recorded.constants[id].clone(),
            Recipe::Input(id) => {
                if let Shape::Scalar(ty) = self.recorded.inputs[id].shape {
                    let slot = self.scalar_input_slots[id]
                        .ok_or_else(|| Error::Type("missing scalar input slot".into()))?;
                    Captured::Word(ty, self.scalars[slot]).value()?
                } else {
                    self.wide_inputs[id]
                        .as_ref()
                        .ok_or_else(|| Error::Type("shape-only input used as a value".into()))?
                        .clone()
                }
            }
            Recipe::Scalar { value, ty } => {
                let value = self.scalar(value)?;
                match ty {
                    ScalarType::Bool => Value::Bool(value != 0),
                    ScalarType::Bits(width) => Value::bits(width.into(), value),
                    ScalarType::Integer => Value::int(value as i128),
                }
            }
            Recipe::Struct(fields) => Value::Struct(
                fields
                    .into_iter()
                    .map(|(name, value)| Ok((name, self.value(interpreter, value)?)))
                    .collect::<Result<_>>()?,
            ),
            Recipe::Union(tag, payload) => {
                Value::Union(tag, Box::new(self.value(interpreter, payload)?))
            }
            Recipe::Vector(values) => Value::Vector(
                values
                    .into_iter()
                    .map(|value| self.value(interpreter, value))
                    .collect::<Result<_>>()?,
            ),
            Recipe::Deferred {
                operation, args, ..
            } => {
                let args = args
                    .into_iter()
                    .map(|id| self.value(interpreter, id))
                    .collect::<Result<Vec<_>>>()?;
                match operation {
                    DeferredOperation::Intrinsic { name, width } => {
                        interpreter.intrinsic(&name, width, &args)?
                    }
                    DeferredOperation::Native { operation, width } => {
                        interpreter.native(&operation, width, &args)?
                    }
                }
            }
        })
    }

    fn bindings<M: Memory>(
        &mut self,
        interpreter: &mut Interpreter<'_, M>,
        bindings: &[Binding],
    ) -> Result<()> {
        let slots = self
            .binding_slots
            .get(&(bindings.as_ptr() as usize))
            .expect("compiled global bindings");
        for (binding, slot) in bindings.iter().zip(slots) {
            let word = match self.recorded.block.values[binding.value] {
                Recipe::Scalar { value, ty } => Some((ty, self.scalar(value)?)),
                Recipe::Constant(id) => match &self.recorded.constants[id] {
                    Value::Bool(value) => Some((ScalarType::Bool, u128::from(*value))),
                    Value::Bits { width, value } if *width <= 128 => value
                        .to_u128()
                        .filter(|raw| *raw <= mask(*width as u16))
                        .map(|raw| (ScalarType::Bits(*width as u16), raw)),
                    Value::Integer(value) => value
                        .to_i128()
                        .map(|raw| (ScalarType::Integer, raw as u128)),
                    _ => None,
                },
                Recipe::Input(id) => match self.recorded.inputs[id].shape {
                    Shape::Scalar(ty) => {
                        self.scalar_input_slots[id].map(|slot| (ty, self.scalars[slot]))
                    }
                    _ => None,
                },
                _ => None,
            };
            if let Some((ty, raw)) = word
                && interpreter.globals.write_native_slot(*slot, ty, raw)
            {
                continue;
            }
            if matches!(
                self.recorded.block.values[binding.value],
                Recipe::Struct(_) | Recipe::Union(..) | Recipe::Vector(_)
            ) && self.update_value(binding.value, interpreter.globals.native_value_mut(*slot))?
            {
                continue;
            }
            let value = self.value(interpreter, binding.value)?;
            interpreter.globals.insert(binding.name.clone(), value);
        }
        Ok(())
    }

    fn update_value(&mut self, id: ValueId, target: &mut Value) -> Result<bool> {
        let recorded = self.recorded;
        match (&recorded.block.values[id], target) {
            (Recipe::Scalar { value, ty }, target) => {
                let raw = self.scalar(*value)?;
                let magnitude = if *ty == ScalarType::Integer {
                    (raw as i128).unsigned_abs()
                } else if let ScalarType::Bits(width) = *ty {
                    raw & mask(width)
                } else {
                    raw
                };
                let digits = [
                    magnitude as u32,
                    (magnitude >> 32) as u32,
                    (magnitude >> 64) as u32,
                    (magnitude >> 96) as u32,
                ];
                match (ty, target) {
                    (ScalarType::Bool, Value::Bool(value)) => *value = raw != 0,
                    (
                        ScalarType::Bits(width),
                        Value::Bits {
                            width: actual,
                            value,
                        },
                    ) if u32::from(*width) == *actual => value.assign_from_slice(&digits),
                    (ScalarType::Integer, Value::Integer(value)) => value.assign_from_slice(
                        if (raw as i128) < 0 {
                            num_bigint::Sign::Minus
                        } else {
                            num_bigint::Sign::Plus
                        },
                        &digits,
                    ),
                    _ => return Ok(false),
                }
                Ok(true)
            }
            (Recipe::Constant(id), target) => {
                *target = recorded.constants[*id].clone();
                Ok(true)
            }
            (Recipe::Struct(fields), Value::Struct(target)) if fields.len() == target.len() => {
                for (name, value) in fields {
                    let Some(target) = target.get_mut(name) else {
                        return Ok(false);
                    };
                    if !self.update_value(*value, target)? {
                        return Ok(false);
                    }
                }
                Ok(true)
            }
            (Recipe::Union(tag, value), Value::Union(actual, target)) if tag == actual => {
                self.update_value(*value, target)
            }
            (Recipe::Vector(values), Value::Vector(target)) if values.len() == target.len() => {
                for (value, target) in values.iter().zip(target) {
                    if !self.update_value(*value, target)? {
                        return Ok(false);
                    }
                }
                Ok(true)
            }
            _ => Ok(false),
        }
    }

    fn resume<M: Memory>(
        &mut self,
        interpreter: &mut Interpreter<'_, M>,
        exit: usize,
    ) -> Result<Value> {
        let (frames, returned) = self.materialize_exit(interpreter, exit)?;
        if returned.is_none() {
            interpreter.resume(frames)
        } else {
            interpreter.resume_with_value(frames, returned)
        }
    }

    fn materialize_exit<M: Memory>(
        &mut self,
        interpreter: &mut Interpreter<'_, M>,
        exit: usize,
    ) -> Result<(Vec<RuntimeFrame>, Option<Value>)> {
        let exit = &self.recorded.block.exits[exit];
        self.bindings(interpreter, &exit.globals)?;
        interpreter.globals.flush_native();
        interpreter.fetch_address = exit
            .fetch_address
            .map(|id| self.value(interpreter, id)?.u64())
            .transpose()?;
        let frames = exit
            .frames
            .iter()
            .map(|frame| {
                Ok(RuntimeFrame {
                    function: frame.function.clone(),
                    locals: {
                        let mut locals: Frame = frame
                            .local_names
                            .iter()
                            .enumerate()
                            .filter(|(slot, _)| {
                                frame.declared[*slot / 64] & (1 << (*slot % 64)) != 0
                            })
                            .map(|(_, name)| (name.clone(), Value::Undefined))
                            .collect();
                        for binding in frame.locals.iter() {
                            locals.insert(
                                binding.name.clone(),
                                self.value(interpreter, binding.value)?,
                            );
                        }
                        locals
                    },
                    pc: frame.pc,
                    pending_result: frame.pending_result.clone(),
                    call_site: frame.call_site,
                })
            })
            .collect::<Result<_>>()?;
        let returned = exit
            .returned
            .map(|value| self.value(interpreter, value))
            .transpose()?;
        Ok((frames, returned))
    }

    fn known(&self, id: ValueId) -> bool {
        match &self.recorded.block.values[id] {
            Recipe::Constant(_) => true,
            Recipe::Scalar { value, .. } => self.recorded.block.graph.literal(*value).is_some(),
            Recipe::Struct(fields) => fields.iter().all(|(_, id)| self.known(*id)),
            Recipe::Union(_, value) => self.known(*value),
            Recipe::Vector(values) => values.iter().all(|id| self.known(*id)),
            _ => false,
        }
    }

    fn capture<M: Memory>(
        &mut self,
        interpreter: &mut Interpreter<'_, M>,
        id: ValueId,
        path: &[Projection],
        descriptor: &Input,
        full: bool,
    ) -> Result<Option<Captured>> {
        if let Some((projection, rest)) = path.split_first() {
            let next = match (&self.recorded.block.values[id], projection) {
                (Recipe::Struct(fields), Projection::Field(name)) => fields
                    .iter()
                    .find(|(field, _)| field == name)
                    .map(|(_, id)| *id),
                (Recipe::Vector(values), Projection::Index(index)) => values.get(*index).copied(),
                (Recipe::Union(tag, value), Projection::Payload(expected)) if tag == expected => {
                    Some(*value)
                }
                _ => None,
            };
            if let Some(next) = next {
                return self.capture(interpreter, next, rest, descriptor, full);
            }
        }
        match &self.recorded.block.values[id] {
            Recipe::Scalar { value, ty } if path.is_empty() => {
                let (value, ty) = (*value, *ty);
                let value = self.scalar(value)?;
                Ok(matches!(descriptor.shape, Shape::Scalar(expected) if expected == ty).then_some(()).filter(|_| descriptor.range.is_none_or(|range| range.contains(value as i128))).map(|_| Captured::Word(ty, value)))
            }
            Recipe::Constant(id) => Ok(project_value(&self.recorded.constants[*id], path).and_then(|value| capture_value(descriptor, value, full))),
            Recipe::Struct(fields) if path.is_empty() && !full => Ok(matches!(&descriptor.shape, Shape::Struct(keys) if keys.len() == fields.len() && keys.iter().all(|key| fields.iter().any(|(name, _)| name == key))).then_some(Captured::Verified)),
            Recipe::Vector(values) if path.is_empty() && !full => Ok(matches!(descriptor.shape, Shape::Vector(len) if len == values.len()).then_some(Captured::Verified)),
            Recipe::Union(tag, _) if path.is_empty() && !full => Ok(matches!(&descriptor.shape, Shape::Union(expected) if expected == tag).then_some(Captured::Verified)),
            _ => { let value = self.value(interpreter, id)?; Ok(project_value(&value, path).and_then(|value| capture_value(descriptor, value, full))) }
        }
    }

    fn error(&self, exit: usize, mut source: Error) -> Error {
        let frames = &self.recorded.block.exits[exit].frames;
        for (index, frame) in frames.iter().enumerate().rev() {
            source = Error::At {
                function: frame.function.name.clone(),
                instruction: if index + 1 == frames.len() {
                    frame.pc
                } else {
                    frame.call_site
                },
                source: Box::new(source),
            };
        }
        source
    }
}

impl Trace {
    fn begin_capture(&mut self) {
        self.epoch = self
            .epoch
            .checked_add(1)
            .filter(|epoch| *epoch != u64::MAX)
            .expect("native scalar epoch exhausted");
    }

    fn bind_capture(&mut self, id: usize, value: Captured) {
        match value {
            Captured::Word(ty, raw) => {
                debug_assert!(
                    matches!(self.recorded.inputs[id].shape, Shape::Scalar(expected) if expected == ty)
                );
                if let Some(slot) = self.scalar_input_slots[id] {
                    self.scratch[slot] = raw;
                    self.stamps[slot] = self.epoch;
                }
            }
            Captured::Value(value) => self.wide_inputs[id] = Some(value),
            Captured::Verified => {}
        }
    }
    #[cfg(test)]
    fn record<M: Memory>(
        interpreter: &mut Interpreter<'_, M>,
        function: &Function,
        args: Vec<Value>,
        options: Options,
    ) -> Result<(Self, Value)> {
        Self::record_block(interpreter, function, args, options, 1)
            .map(|(trace, value, _)| (trace, value))
    }

    pub fn record_block<M: Memory>(
        interpreter: &mut Interpreter<'_, M>,
        function: &Function,
        mut args: Vec<Value>,
        options: Options,
        limit: u32,
    ) -> Result<(Self, Value, u32)> {
        let transaction = interpreter.memory.transaction_lock();
        let _atomic = transaction.as_ref().map(|lock| lock.lock());
        let step_argument = options.step_argument;
        interpreter.trace_completed = 0;
        interpreter.globals.flush_native();
        interpreter.recorder = Some(Recorder::new(options.clone()));
        let mut steps = 0;
        let mut value;
        loop {
            let previous_pc = options
                .stop_at_control_flow
                .then(|| {
                    interpreter
                        .globals
                        .get("zPC")
                        .and_then(|value| value.u64().ok())
                })
                .flatten();
            interpreter.fetch_address = None;
            let result = interpreter.run_function(function, args.clone());
            let mut recorder = interpreter.recorder.take().expect("trace recorder");
            value = result?;
            steps += 1;
            interpreter.trace_completed = steps;
            recorder.end_step(interpreter);
            let control_flow = previous_pc.is_some_and(|previous| {
                interpreter
                    .globals
                    .get("zPC")
                    .and_then(|value| value.u64().ok())
                    .is_none_or(|next| {
                        previous.checked_add(2) != Some(next)
                            && previous.checked_add(4) != Some(next)
                    })
            });
            if steps == limit
                || control_flow
                || recorder.dynamic_pc()
                || recorder.stopped()
                || value.boolean().unwrap_or(false)
                || interpreter.watch_hit
            {
                return Ok((
                    Self::from_record(interpreter, recorder.finish(), options, steps),
                    value,
                    steps,
                ));
            }
            if let Some(i) = step_argument {
                args[i] = Value::int(args[i].integer()? + 1);
            }
            interpreter.recorder = Some(recorder);
        }
    }

    fn from_record<M: Memory>(
        interpreter: &mut Interpreter<'_, M>,
        mut recorded: Recorded,
        options: Options,
        steps: u32,
    ) -> Self {
        let mut code = BTreeSet::new();
        recorded.block.code_guards.retain(|guard| {
            code.insert((
                guard.generation,
                guard.identity,
                guard.mapping,
                guard.first_page,
                guard.last_page,
            ))
        });
        let input_slots = recorded
            .inputs
            .iter()
            .map(|descriptor| match &descriptor.source {
                InputSource::Global(name, path) if path.is_empty() => {
                    interpreter.globals.slot(name)
                }
                _ => None,
            })
            .collect();
        let binding_slots = recorded
            .block
            .exits
            .iter()
            .map(|exit| &exit.globals)
            .chain(
                recorded
                    .block
                    .checkpoints
                    .iter()
                    .map(|checkpoint| &checkpoint.globals),
            )
            .map(|bindings| {
                (
                    bindings.as_slice().as_ptr() as usize,
                    bindings
                        .iter()
                        .map(|binding| {
                            interpreter
                                .globals
                                .slot(&binding.name)
                                .expect("recorded global slot")
                        })
                        .collect(),
                )
            })
            .collect();
        let context_slots: Vec<usize> = recorded
            .block
            .context_guards
            .iter()
            .map(|guard| interpreter.globals.slot(&guard.name).expect("context slot"))
            .collect();
        for slot in &context_slots {
            interpreter.globals.watch_context(*slot);
        }
        let (slots, slot_count) = vmlab_hv_tcg_ir::native::linear::slots(&recorded.block.graph);
        let mut scalar_input_slots = vec![None; recorded.inputs.len()];
        for (id, scalar) in recorded.block.graph.scalars.iter().enumerate() {
            if let Operation::Input(input) = scalar.operation {
                debug_assert!(
                    matches!(recorded.inputs[input].shape, Shape::Scalar(ty) if ty == scalar.ty)
                );
                let previous = scalar_input_slots[input].replace(slots[id]);
                assert!(
                    previous.is_none_or(|previous| previous == slots[id]),
                    "one slot per scalar input"
                );
            }
        }
        let wide_inputs = (0..recorded.inputs.len()).map(|_| None).collect();
        let stages: Vec<Stage> = vmlab_hv_tcg_ir::native::schedule::schedule(&recorded.block)
            .into_iter()
            .map(|stage| lower_stage(&recorded.block.graph, &slots, stage))
            .collect();
        let checkpoint_stages = recorded
            .block
            .checkpoints
            .iter()
            .map(|checkpoint| {
                let roots: Vec<_> = std::iter::once(checkpoint.waiting)
                    .chain(checkpoint.globals.iter().map(|binding| binding.value))
                    .collect();
                lower_stage(
                    &recorded.block.graph,
                    &slots,
                    vmlab_hv_tcg_ir::native::schedule::values(&recorded.block, &roots),
                )
            })
            .collect();
        let waiting: Vec<Option<bool>> = recorded
            .block
            .checkpoints
            .iter()
            .map(
                |checkpoint| match recorded.block.values[checkpoint.waiting] {
                    Recipe::Constant(id) => recorded.constants[id].boolean().ok(),
                    Recipe::Scalar {
                        value,
                        ty: ScalarType::Bool,
                    } => recorded.block.graph.literal(value).map(|v| v != 0),
                    _ => None,
                },
            )
            .collect();
        let waiting_is_false = waiting.iter().all(|waiting| *waiting == Some(false));
        let mut retired = 0;
        let mut event_steps = Vec::with_capacity(recorded.block.events.len());
        let mut fast_events = Vec::new();
        for (index, event) in recorded.block.events.iter().enumerate() {
            event_steps.push(retired);
            if let Event::Checkpoint(id) = event {
                retired = recorded.block.checkpoints[*id].completed_steps;
                if *id + 1 == recorded.block.checkpoints.len() && recorded.stop_reason.is_none() {
                    fast_events.push(index);
                }
            } else {
                fast_events.push(index);
            }
        }
        let fused_guards: Vec<Option<vmlab_hv_tcg_ir::native::linear::Guard>> = recorded
            .block
            .events
            .iter()
            .map(|event| match *event {
                Event::Guard {
                    condition,
                    expected,
                    ..
                } => vmlab_hv_tcg_ir::native::linear::guard(
                    &recorded.block.graph,
                    condition,
                    expected,
                )
                .map(|mut guard| {
                    guard.left = slots[guard.left];
                    guard.right = slots[guard.right];
                    guard
                }),
                _ => None,
            })
            .collect();
        let mut fast_code = Vec::new();
        for &index in &fast_events {
            use vmlab_hv_tcg_ir::native::packed::Packed;
            fast_code.extend_from_slice(&stages[index]);
            let instruction = match recorded.block.events[index] {
                Event::Guard {
                    condition,
                    expected,
                    ..
                } => {
                    if let Some(guard) = fused_guards[index] {
                        Packed::guard(index, guard)
                    } else {
                        Packed::boolean_guard(index, slots[condition], expected)
                    }
                }
                _ => Packed::event(index),
            };
            fast_code.push(instruction.expect("bounded packed event"));
        }
        let mut trace = Self {
            recorded,
            revision: None,
            context_slots,
            stages,
            checkpoint_stages,
            fused_guards,
            scratch: Vec::new(),
            stamps: Vec::new(),
            epoch: 0,
            waiting,
            global_identity: interpreter.globals.identity(),
            input_values: Vec::new(),
            successful_runs: 0,
            trained_steps: steps,
            cache_units: 0,
            last_used: 0,
            options,
            fragments: RefCell::new(BTreeMap::new()),
            fragment_units: Cell::new(0),
            branch_depth: 0,
            input_slots,
            binding_slots,
            pc_slot: interpreter.globals.slot("zPC"),
            compact_loop: std::sync::OnceLock::new(),
            wide_inputs,
            scalar_input_slots,
            slots,
            fast_events,
            fast_code,
            event_steps,
            waiting_is_false,
        };
        trace.input_values = vec![false; trace.recorded.inputs.len()];
        for recipe in &trace.recorded.block.values {
            if let Recipe::Input(id) = recipe {
                trace.input_values[*id] = true;
            }
        }
        trace.cache_units = trace.calculate_cache_units();
        trace.scratch.resize(slot_count, 0);
        trace.stamps.resize(slot_count, 0);
        for (id, scalar) in trace.recorded.block.graph.scalars.iter().enumerate() {
            if let Operation::Constant(value) = scalar.operation {
                trace.scratch[trace.slots[id]] = value;
                trace.stamps[trace.slots[id]] = u64::MAX;
            }
        }
        trace
    }

    fn code_valid(&self, memory: &impl Memory) -> bool {
        self.recorded.block.code_guards.iter().all(|guard| {
            memory
                .code_stamp(guard.address, guard.len)
                .is_some_and(|stamp| {
                    stamp.generation == guard.generation
                        && stamp.identity == guard.identity
                        && stamp.mapping == guard.mapping
                        && stamp.first_page == guard.first_page
                        && stamp.last_page == guard.last_page
                })
        })
    }

    pub fn run<M: Memory>(
        &mut self,
        interpreter: &mut Interpreter<'_, M>,
        args: &[Value],
        limit: u32,
    ) -> Result<Outcome> {
        self.run_inputs(interpreter, args, None, limit)
    }

    pub fn run_step<M: Memory>(
        &mut self,
        interpreter: &mut Interpreter<'_, M>,
        steps: u64,
        limit: u32,
    ) -> Result<Outcome> {
        self.run_inputs(interpreter, &[], Some(steps), limit)
    }

    fn run_inputs<M: Memory>(
        &mut self,
        interpreter: &mut Interpreter<'_, M>,
        args: &[Value],
        steps: Option<u64>,
        limit: u32,
    ) -> Result<Outcome> {
        interpreter.trace_completed = 0;
        let transaction = interpreter.memory.transaction_lock();
        let _atomic = transaction.as_ref().map(|lock| lock.lock());
        if !self.entry_valid(interpreter) {
            return Ok(Outcome::Miss);
        }
        self.begin_capture();
        for id in 0..self.recorded.inputs.len() {
            let descriptor = &self.recorded.inputs[id];
            if let Some(steps) = steps
                && matches!(descriptor.source, InputSource::Argument(0))
            {
                let Some(value) =
                    capture_global_word(descriptor, (ScalarType::Integer, u128::from(steps)))
                else {
                    return Ok(Outcome::Miss);
                };
                self.bind_capture(id, value);
                continue;
            }
            if let Some(word) =
                self.input_slots[id].and_then(|slot| interpreter.globals.native_slot(slot))
            {
                let Some(value) = capture_global_word(descriptor, word) else {
                    return Ok(Outcome::Miss);
                };
                self.bind_capture(id, value);
                continue;
            }
            let Some(value) = input(&descriptor.source, interpreter, args) else {
                return Ok(Outcome::Miss);
            };
            let Some(value) = capture_value(descriptor, value, self.input_values[id]) else {
                return Ok(Outcome::Miss);
            };
            self.bind_capture(id, value);
        }
        self.run_captured(interpreter, limit)
    }

    fn entry_valid<M: Memory>(&mut self, interpreter: &Interpreter<'_, M>) -> bool {
        if self.global_identity != interpreter.globals.identity()
            || !self.code_valid(&interpreter.memory)
        {
            return false;
        }
        if let Some(pc) = self.options.pc
            && !match self
                .pc_slot
                .and_then(|slot| interpreter.globals.native_slot(slot))
            {
                Some((ScalarType::Bits(64), raw)) => raw == u128::from(pc),
                Some(_) => false,
                None => {
                    matches!(interpreter.globals.get("zPC"), Some(Value::Bits { width: 64, value }) if value.to_u64() == Some(pc))
                }
            }
        {
            return false;
        }
        if self.revision != Some(interpreter.globals.context_revision()) {
            for (guard, slot) in self
                .recorded
                .block
                .context_guards
                .iter_mut()
                .zip(&self.context_slots)
            {
                let version = interpreter.globals.version(*slot);
                if guard.version != version {
                    if !interpreter
                        .globals
                        .native_matches(&guard.name, &self.recorded.constants[guard.expected])
                    {
                        if std::env::var_os("VMLAB_TCG_TRACE_PC").is_some() {
                            eprintln!(
                                "entry context mismatch pc={:?}: {}",
                                self.options.pc, guard.name
                            );
                        }
                        return false;
                    }
                    guard.version = version;
                }
            }
            self.revision = Some(interpreter.globals.context_revision());
        }
        true
    }

    pub fn matches_entry<M: Memory>(
        &mut self,
        interpreter: &Interpreter<'_, M>,
        args: &[Value],
    ) -> bool {
        self.entry_valid(interpreter)
            && self
                .recorded
                .inputs
                .iter()
                .enumerate()
                .all(|(id, descriptor)| {
                    if let Some(word) =
                        self.input_slots[id].and_then(|slot| interpreter.globals.native_slot(slot))
                    {
                        return capture_global_word(descriptor, word).is_some();
                    }
                    input(&descriptor.source, interpreter, args)
                        .and_then(|value| capture_value(descriptor, value, false))
                        .is_some()
                })
    }

    fn run_captured<M: Memory>(
        &mut self,
        interpreter: &mut Interpreter<'_, M>,
        limit: u32,
    ) -> Result<Outcome> {
        let mut evaluation = Evaluation {
            recorded: &self.recorded,
            wide_inputs: std::mem::take(&mut self.wide_inputs),
            scalar_input_slots: &self.scalar_input_slots,
            scalars: std::mem::take(&mut self.scratch),
            stamps: std::mem::take(&mut self.stamps),
            epoch: self.epoch,
            completed_steps: 0,
            binding_slots: &self.binding_slots,
            slots: &self.slots,
        };
        let outcome = self.execute(interpreter, &mut evaluation, limit);
        interpreter.trace_completed = evaluation.completed_steps;
        self.scratch = evaluation.scalars;
        self.stamps = evaluation.stamps;
        self.wide_inputs = evaluation.wide_inputs;
        if matches!(outcome, Ok(Outcome::Returned(_, _))) {
            self.successful_runs = self.successful_runs.saturating_add(1);
        }
        outcome
    }

    fn run_bound<M: Memory>(
        &mut self,
        interpreter: &mut Interpreter<'_, M>,
        parent: &mut Evaluation<'_>,
        exit: usize,
    ) -> Result<Outcome> {
        if !self.entry_valid(interpreter) {
            return Ok(Outcome::Miss);
        }
        self.begin_capture();
        for id in 0..self.recorded.inputs.len() {
            let descriptor = &self.recorded.inputs[id];
            let value = match &descriptor.source {
                InputSource::Local(frame, name, path) => {
                    let symbol = parent.recorded.block.exits[exit]
                        .frames
                        .get(*frame)
                        .and_then(|frame| frame.locals.iter().find(|binding| binding.name == *name))
                        .map(|binding| binding.value);
                    if let Some(symbol) = symbol {
                        parent.capture(
                            interpreter,
                            symbol,
                            path,
                            descriptor,
                            self.input_values[id],
                        )?
                    } else {
                        None
                    }
                }
                InputSource::FetchAddress => {
                    if let Some(symbol) = parent.recorded.block.exits[exit].fetch_address {
                        if let Some(value) =
                            parent.capture(interpreter, symbol, &[], descriptor, false)?
                        {
                            Some(value)
                        } else {
                            parent
                                .value(interpreter, symbol)?
                                .u64()
                                .ok()
                                .map(|address| Captured::Word(ScalarType::Bits(64), address.into()))
                        }
                    } else {
                        None
                    }
                }
                InputSource::Global(_, path)
                    if path.is_empty()
                        && self.input_slots[id]
                            .and_then(|slot| interpreter.globals.native_slot(slot))
                            .is_some() =>
                {
                    capture_global_word(
                        descriptor,
                        interpreter
                            .globals
                            .native_slot(self.input_slots[id].unwrap())
                            .unwrap(),
                    )
                }
                _ => input(&descriptor.source, interpreter, &[])
                    .and_then(|value| capture_value(descriptor, value, self.input_values[id])),
            };
            let Some(value) = value else {
                return Ok(Outcome::Miss);
            };
            self.bind_capture(id, value);
        }
        self.run_captured(interpreter, 1)
    }

    fn guard_failed<M: Memory>(
        &self,
        interpreter: &mut Interpreter<'_, M>,
        evaluation: &mut Evaluation<'_>,
        exit: usize,
    ) -> Result<Outcome> {
        let snapshot = &self.recorded.block.exits[exit];
        let completed = snapshot.completed_steps;
        if self.branch_depth >= 16 || snapshot.returned.is_some() {
            return evaluation
                .resume(interpreter, exit)
                .map(|value| Outcome::Resumed(value, completed + 1));
        }
        // The alternate path starts after this prefix. Publish its state once;
        // its ordered memory events have already executed and are never replayed.
        evaluation.bindings(interpreter, &snapshot.globals)?;
        let mut fragments = self.fragments.borrow_mut();
        let variants = fragments.entry(exit).or_default();
        for fragment in variants.iter_mut() {
            let before = fragment.cache_units();
            let result = fragment.run_bound(interpreter, evaluation, exit);
            self.fragment_units
                .set(self.fragment_units.get() - before + fragment.cache_units());
            match result? {
                Outcome::Miss => {}
                Outcome::Returned(value, steps) => {
                    return Ok(Outcome::Returned(value, completed + steps));
                }
                Outcome::Resumed(value, steps) => {
                    return Ok(Outcome::Resumed(value, completed + steps));
                }
            }
        }
        if self.cache_units() > 500_000 {
            return evaluation
                .resume(interpreter, exit)
                .map(|value| Outcome::Resumed(value, completed + 1));
        }
        let dynamic: Vec<BTreeSet<String>> = snapshot
            .frames
            .iter()
            .map(|frame| {
                frame
                    .locals
                    .iter()
                    .filter(|binding| !evaluation.known(binding.value))
                    .map(|binding| binding.name.clone())
                    .collect()
            })
            .collect();
        let known_globals: Frame = snapshot
            .globals
            .iter()
            .filter_map(|binding| {
                if evaluation.known(binding.value) {
                    Some(
                        evaluation
                            .value(interpreter, binding.value)
                            .map(|value| (binding.name.clone(), value)),
                    )
                } else {
                    None
                }
            })
            .collect::<Result<_>>()?;
        let (frames, returned) = evaluation.materialize_exit(interpreter, exit)?;
        let mut options = self.options.clone();
        options.pc = interpreter
            .globals
            .get("zPC")
            .and_then(|value| value.u64().ok());
        options.step_argument = None;
        let fetch_address = interpreter.fetch_address;
        let recorder = Recorder::seed(
            interpreter,
            options.clone(),
            &frames,
            &dynamic,
            &known_globals,
            fetch_address,
        );
        interpreter.recorder = Some(recorder);
        let result = interpreter.resume_with_value(frames, returned);
        let mut recorder = interpreter.recorder.take().expect("continuation recorder");
        let value = result?;
        recorder.end_step(interpreter);
        let mut fragment = Self::from_record(interpreter, recorder.finish(), options, 1);
        fragment.branch_depth = self.branch_depth + 1;
        let units = fragment.cache_units();
        if units <= 250_000 {
            if variants.len() == 4 {
                self.fragment_units
                    .set(self.fragment_units.get() - variants.remove(0).cache_units());
            }
            self.fragment_units.set(self.fragment_units.get() + units);
            variants.push(fragment);
        }
        Ok(Outcome::Resumed(value, completed + 1))
    }

    fn execute<M: Memory>(
        &self,
        interpreter: &mut Interpreter<'_, M>,
        evaluation: &mut Evaluation<'_>,
        limit: u32,
    ) -> Result<Outcome> {
        let mut fast_code = self.fast_code.iter();
        let mut full_next = (!(self.waiting_is_false && limit >= self.trained_steps)).then_some(0);
        loop {
            let event_index = if let Some(index) = full_next {
                full_next = Some(index + 1);
                index
            } else {
                loop {
                    use vmlab_hv_tcg_ir::native::packed::Control;
                    let Some(instruction) = fast_code.next() else {
                        return Err(Error::Type("trace has no terminal event".into()));
                    };
                    match instruction.execute(
                        &mut evaluation.scalars,
                        &mut evaluation.stamps,
                        evaluation.epoch,
                    ) {
                        Control::Continue => {}
                        Control::Event(index) => break index,
                        Control::GuardFailed(index) => {
                            evaluation.completed_steps = self.event_steps[index];
                            let Event::Guard { exit, .. } = self.recorded.block.events[index]
                            else {
                                unreachable!("packed guard event");
                            };
                            return self.guard_failed(interpreter, evaluation, exit);
                        }
                        Control::InvalidArithmetic => {
                            if let Some(index) = fast_code
                                .clone()
                                .find_map(|instruction| instruction.event_index())
                            {
                                evaluation.completed_steps = self.event_steps[index];
                            }
                            return Err(Error::Type("native scalar domain".into()));
                        }
                    }
                }
            };
            let Some(event) = self.recorded.block.events.get(event_index) else {
                break;
            };
            let stage = &self.stages[event_index];
            evaluation.completed_steps = self.event_steps[event_index];
            if full_next.is_some() {
                for instruction in stage {
                    evaluation.linear(instruction)?;
                }
            }
            match *event {
                Event::Guard {
                    condition,
                    expected,
                    exit,
                } => {
                    let matches = if let Some(guard) = self.fused_guards[event_index] {
                        guard.matches(
                            evaluation.scalars[guard.left],
                            evaluation.scalars[guard.right],
                        )
                    } else {
                        (evaluation.scalar(condition)? != 0) == expected
                    };
                    if !matches {
                        return self.guard_failed(interpreter, evaluation, exit);
                    }
                }
                Event::ReadMemory {
                    address,
                    bytes,
                    result,
                    error,
                    ..
                } => {
                    let address = u64::try_from(evaluation.scalar(address)?)
                        .map_err(|_| Error::Type("native physical address".into()))?;
                    let mut data = [0; 16];
                    if let Err(source) = interpreter
                        .memory
                        .read(address, &mut data[..bytes as usize])
                    {
                        evaluation
                            .bindings(interpreter, &self.recorded.block.exits[error].globals)?;
                        return Err(evaluation.error(error, source));
                    }
                    let result = evaluation.slots[result];
                    evaluation.scalars[result] = u128::from_le_bytes(data);
                    evaluation.stamps[result] = evaluation.epoch;
                }
                Event::WriteMemory {
                    address,
                    value,
                    bytes,
                    error,
                    invalidated,
                    ..
                } => {
                    let address = u64::try_from(evaluation.scalar(address)?)
                        .map_err(|_| Error::Type("native physical address".into()))?;
                    let data = evaluation.scalar(value)?.to_le_bytes();
                    if let Err(source) = interpreter.memory.write(address, &data[..bytes as usize])
                    {
                        evaluation
                            .bindings(interpreter, &self.recorded.block.exits[error].globals)?;
                        return Err(evaluation.error(error, source));
                    }
                    interpreter.reservation = None;
                    if interpreter.watch_store.is_some_and(|watch| {
                        watch
                            .checked_sub(address)
                            .is_some_and(|delta| delta < u64::from(bytes))
                    }) {
                        interpreter.watch_hit = true;
                        full_next = Some(event_index + 1);
                    }
                    if !self.code_valid(&interpreter.memory) {
                        return evaluation.resume(interpreter, invalidated).map(|value| {
                            Outcome::Resumed(
                                value,
                                self.recorded.block.exits[invalidated].completed_steps + 1,
                            )
                        });
                    }
                }
                Event::Checkpoint(id) => {
                    let checkpoint = &self.recorded.block.checkpoints[id];
                    evaluation.completed_steps = checkpoint.completed_steps;
                    if !interpreter.watch_hit
                        && checkpoint.completed_steps != limit
                        && self.waiting[id] == Some(false)
                        && (id + 1 != self.recorded.block.checkpoints.len()
                            || self.recorded.stop_reason.is_some())
                    {
                        continue;
                    }
                    for instruction in &self.checkpoint_stages[id] {
                        let output = instruction.output as usize;
                        if evaluation.stamps[output] != evaluation.epoch
                            && evaluation.stamps[output] != u64::MAX
                        {
                            evaluation.linear(instruction)?;
                        }
                    }
                    let waiting = if let Some(waiting) = self.waiting[id] {
                        Value::Bool(waiting)
                    } else {
                        evaluation.value(interpreter, checkpoint.waiting)?
                    };
                    if interpreter.watch_hit
                        || checkpoint.completed_steps == limit
                        || waiting.boolean().unwrap_or(false)
                        || id + 1 == self.recorded.block.checkpoints.len()
                            && self.recorded.stop_reason.is_none()
                    {
                        evaluation.bindings(interpreter, &checkpoint.globals)?;
                        return Ok(Outcome::Returned(waiting, checkpoint.completed_steps));
                    }
                }
                Event::Exit(id) => {
                    return evaluation.resume(interpreter, id).map(|value| {
                        Outcome::Resumed(value, self.recorded.block.exits[id].completed_steps + 1)
                    });
                }
            }
        }
        Err(Error::Type("trace has no terminal event".into()))
    }

    pub fn stop_reason(&self) -> Option<&str> {
        self.recorded.stop_reason.as_deref()
    }

    pub fn unbounded_globals(&self) -> &BTreeSet<String> {
        &self.options.unbounded_globals
    }

    pub fn failed_ranges(&self, interpreter: &Interpreter<'_, impl Memory>) -> Vec<String> {
        if self.global_identity != interpreter.globals.identity() {
            return Vec::new();
        }
        self.recorded
            .inputs
            .iter()
            .enumerate()
            .filter_map(|(id, descriptor)| {
                let InputSource::Global(name, path) = &descriptor.source else {
                    return None;
                };
                let Shape::Scalar(ScalarType::Bits(64)) = descriptor.shape else {
                    return None;
                };
                let range = descriptor.range?;
                if !path.is_empty() {
                    return None;
                }
                let value = if let Some((ScalarType::Bits(64), raw)) =
                    self.input_slots[id].and_then(|slot| interpreter.globals.native_slot(slot))
                {
                    Some(raw)
                } else {
                    input(&descriptor.source, interpreter, &[])
                        .and_then(|value| raw(value, ScalarType::Bits(64)))
                }?;
                (!range.contains(value as i128)).then(|| name.clone())
            })
            .collect()
    }

    pub fn dump(&self) {
        eprintln!(
            "known waiting: {}/{}",
            self.waiting
                .iter()
                .filter(|waiting| **waiting == Some(false))
                .count(),
            self.waiting.len()
        );
        eprintln!(
            "trace pc={:?} steps={} inputs={} scheduled={}",
            self.options.pc,
            self.trained_steps,
            self.recorded.inputs.len(),
            self.stages.iter().map(Vec::len).sum::<usize>()
        );
        for (id, input) in self.recorded.inputs.iter().enumerate() {
            eprintln!("input {id}: {input:?}");
        }
        for (id, scalar) in self.recorded.block.graph.scalars.iter().enumerate() {
            eprintln!("scalar {id}: {scalar:?}");
        }
        for event in &self.recorded.block.events {
            eprintln!("event: {event:?}");
        }
        if let Some(checkpoint) = self.recorded.block.checkpoints.last() {
            for binding in checkpoint.globals.iter() {
                eprintln!(
                    "global {}: {:?}",
                    binding.name, self.recorded.block.values[binding.value]
                );
            }
        }
    }
    pub fn scalar_count(&self) -> usize {
        self.recorded.block.graph.scalars.len()
    }

    pub fn profile(&self, pc: u64) -> crate::TraceProfile {
        crate::TraceProfile {
            pc,
            calls: self.successful_runs,
            instructions: self.trained_steps,
            operations: self
                .fast_events
                .iter()
                .map(|event| self.stages[*event].len())
                .sum(),
            guards: self
                .fast_events
                .iter()
                .filter(|event| matches!(self.recorded.block.events[**event], Event::Guard { .. }))
                .count(),
            inputs: self.recorded.inputs.len(),
            outputs: self
                .recorded
                .block
                .checkpoints
                .last()
                .map_or(0, |checkpoint| checkpoint.globals.len()),
        }
    }

    fn promotion_target(&self, limit: u32) -> u32 {
        if self.recorded.dynamic_pc {
            return limit.min(1);
        }
        let compact_loop = *self.compact_loop.get_or_init(|| {
            self.options.pc.is_some_and(|entry| {
                let guards = &self.recorded.block.code_guards;
                let low = guards.iter().map(|guard| guard.address).min();
                let high = guards
                    .iter()
                    .map(|guard| guard.address.saturating_add(guard.len as u64))
                    .max();
                let next = self
                    .recorded
                    .block
                    .checkpoints
                    .last()
                    .and_then(|checkpoint| {
                        checkpoint
                            .globals
                            .iter()
                            .find(|binding| binding.name == "zPC")
                    })
                    .and_then(|binding| match self.recorded.block.values[binding.value] {
                        Recipe::Constant(id) => self.recorded.constants[id].u64().ok(),
                        Recipe::Scalar { value, .. } => self
                            .recorded
                            .block
                            .graph
                            .literal(value)
                            .and_then(|value| value.try_into().ok()),
                        _ => None,
                    });
                low.zip(high).zip(next).is_some_and(|((low, high), next)| {
                    high.saturating_sub(low) <= 256
                        && next >= low.saturating_sub(16)
                        && next <= entry
                })
            })
        });
        if compact_loop { limit } else { limit.min(1) }
    }

    pub fn promote(&self, limit: u32) -> bool {
        let target = self.promotion_limit(256);
        self.successful_runs >= 8
            && self.trained_steps < target
            && limit >= target
            && self.recorded.stop_reason.is_none()
            && self.stages.iter().map(Vec::len).sum::<usize>() <= self.trained_steps as usize * 32
            && self.cache_units.saturating_mul(target as usize) / self.trained_steps as usize
                <= 100_000
    }

    pub fn promotion_limit(&self, limit: u32) -> u32 {
        self.promotion_target(limit)
            .min(self.trained_steps.saturating_mul(4))
    }

    pub fn cache_units(&self) -> usize {
        self.cache_units + self.fragment_units.get()
    }

    pub fn cache_breakdown(&self) -> [usize; 4] {
        let block = &self.recorded.block;
        let mut seen = BTreeSet::new();
        let locals = block
            .exits
            .iter()
            .flat_map(|exit| &exit.frames)
            .filter_map(|frame| {
                seen.insert(std::sync::Arc::as_ptr(&frame.locals) as usize)
                    .then_some(frame.locals.len())
            })
            .sum();
        let globals = block
            .exits
            .iter()
            .map(|exit| &exit.globals)
            .chain(
                block
                    .checkpoints
                    .iter()
                    .map(|checkpoint| &checkpoint.globals),
            )
            .filter_map(|bindings| {
                seen.insert(std::sync::Arc::as_ptr(bindings) as usize)
                    .then_some(bindings.len())
            })
            .sum();
        [
            block.values.len(),
            block.graph.scalars.len(),
            locals,
            globals,
        ]
    }

    fn calculate_cache_units(&self) -> usize {
        fn value_units(value: &Value) -> usize {
            1 + match value {
                Value::Struct(fields) => fields.values().map(value_units).sum(),
                Value::Union(_, payload) => value_units(payload),
                Value::Vector(values) => values.iter().map(value_units).sum(),
                Value::Bits { width, .. } => *width as usize / 128,
                Value::Integer(n) => n.bits() as usize / 128,
                Value::String(s) => s.len() / 16,
                _ => 0,
            }
        }
        let block = &self.recorded.block;
        let breakdown = self.cache_breakdown();
        block.graph.scalars.len()
            + self.fast_events.len()
            + self.fast_code.len()
            + self.event_steps.len()
            + self.slots.len()
            + self.stages.iter().map(Vec::len).sum::<usize>()
            + self.checkpoint_stages.iter().map(Vec::len).sum::<usize>()
            + block.values.len()
            + block.events.len()
            + breakdown[2]
            + breakdown[3]
            + block
                .exits
                .iter()
                .flat_map(|exit| &exit.frames)
                .map(|frame| frame.declared.len() + 1)
                .sum::<usize>()
            + self
                .recorded
                .constants
                .iter()
                .map(value_units)
                .sum::<usize>()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::FlatMemory;
    use std::collections::{BTreeMap, BTreeSet};
    use vmlab_hv_tcg_ir::{Expr, Location, Op, Program, Target, Type};

    #[test]
    fn replacing_globals_cannot_alias_a_context_version() {
        let function = Function {
            name: "context".into(),
            args: vec![],
            result: Type::Bool,
            locals: BTreeMap::new(),
            code: vec![
                Op::Assign(Location::Id("return".into()), Expr::Id("mode".into())),
                Op::Return,
            ],
        };
        let program = Program {
            globals: BTreeMap::from([("mode".into(), Type::Bool)]),
            ..Program::default()
        };
        let mut interpreter = Interpreter::new(&program, FlatMemory::new(0, vec![]));
        interpreter.initialize().unwrap();
        let (mut trace, _) =
            Trace::record(&mut interpreter, &function, vec![], Options::default()).unwrap();
        let mut replacement = crate::Globals::default();
        replacement.insert("mode".into(), Value::Bool(true));
        replacement.insert("have_exception".into(), Value::Bool(false));
        replacement.insert("current_exception".into(), Value::Undefined);
        replacement.insert("throw_location".into(), Value::String(String::new()));
        assert_eq!(interpreter.globals.revision(), replacement.revision());
        interpreter.globals = replacement;
        assert!(matches!(
            trace.run(&mut interpreter, &[], 1).unwrap(),
            Outcome::Miss
        ));
    }

    #[test]
    fn native_remainder_preserves_signed_and_unsigned_integer_domains() {
        let function = Function {
            name: "remainder".into(),
            args: vec![("n".into(), Type::Integer(None))],
            result: Type::Integer(None),
            locals: BTreeMap::new(),
            code: vec![
                Op::Assign(
                    Location::Id("return".into()),
                    Expr::Op(
                        "tmod_int".into(),
                        None,
                        vec![Expr::Id("n".into()), Expr::Integer("8".into(), None)],
                    ),
                ),
                Op::Return,
            ],
        };
        let program = Program::default();
        let mut interpreter = Interpreter::new(&program, FlatMemory::new(0, vec![]));
        for (initial, values) in [
            (17, vec![0, 1, 7, 8, 9, u64::MAX as i128]),
            (-17, vec![i64::MIN as i128, -17, -1, 0, i64::MAX as i128]),
        ] {
            let (mut trace, _) = Trace::record(
                &mut interpreter,
                &function,
                vec![Value::int(initial)],
                Options::default(),
            )
            .unwrap();
            assert_eq!(trace.stop_reason(), None);
            for value in values {
                let args = vec![Value::int(value)];
                let expected = interpreter.run_function(&function, args.clone()).unwrap();
                assert!(
                    matches!(trace.run(&mut interpreter, &args, 1).unwrap(), Outcome::Returned(actual, 1) if actual == expected),
                    "{value}"
                );
            }
        }
    }

    #[test]
    fn a_dead_local_still_shadows_a_global_when_a_child_returns() {
        let child = Function {
            name: "child".into(),
            args: vec![],
            result: Type::Integer(None),
            locals: BTreeMap::new(),
            code: vec![
                Op::Branch(Expr::Id("decision".into()), 3),
                Op::Assign(
                    Location::Id("return".into()),
                    Expr::Integer("7".into(), None),
                ),
                Op::Jump(4),
                Op::Assign(
                    Location::Id("return".into()),
                    Expr::Integer("9".into(), None),
                ),
                Op::Return,
            ],
        };
        let root = Function {
            name: "root".into(),
            args: vec![],
            result: Type::Integer(None),
            locals: BTreeMap::new(),
            code: vec![
                Op::Declare("slot".into(), Type::Integer(None), None),
                Op::Call(
                    Location::Id("slot".into()),
                    Target::Function("child".into()),
                    vec![],
                ),
                Op::Assign(
                    Location::Id("slot".into()),
                    Expr::Integer("42".into(), None),
                ),
                Op::Assign(Location::Id("return".into()), Expr::Id("slot".into())),
                Op::Return,
            ],
        };
        let program = Program {
            functions: BTreeMap::from([("child".into(), child)]),
            globals: BTreeMap::from([
                ("slot".into(), Type::Integer(None)),
                ("decision".into(), Type::Bool),
            ]),
            ..Program::default()
        };
        let mut interpreter = Interpreter::new(&program, FlatMemory::new(0, vec![]));
        interpreter.initialize().unwrap();
        interpreter.globals.insert("slot".into(), Value::int(999));
        let options = Options {
            dynamic_globals: BTreeSet::from(["decision".into()]),
            ..Options::default()
        };
        let (mut trace, _) = Trace::record(&mut interpreter, &root, vec![], options).unwrap();
        let caller = &trace.recorded.block.exits[0].frames[0];
        assert!(!caller.locals.iter().any(|binding| binding.name == "slot"));
        interpreter
            .globals
            .insert("decision".into(), Value::Bool(true));
        assert!(
            matches!(trace.run(&mut interpreter, &[], 1).unwrap(), Outcome::Resumed(value, 1) if value == Value::int(42))
        );
        assert_eq!(interpreter.globals["slot"], Value::int(999));
    }

    #[test]
    fn integer_identity_folding_keeps_the_sail_value_kind() {
        let program = Program::default();
        let mut interpreter = Interpreter::new(&program, FlatMemory::new(0, vec![]));
        for width in [8, 64, 128] {
            let function = Function {
                name: "identity".into(),
                args: vec![("n".into(), Type::Bits(Some(width)))],
                result: Type::Integer(None),
                locals: BTreeMap::new(),
                code: vec![
                    Op::Assign(
                        Location::Id("return".into()),
                        Expr::Op(
                            "add_int".into(),
                            None,
                            vec![Expr::Id("n".into()), Expr::Integer("0".into(), None)],
                        ),
                    ),
                    Op::Return,
                ],
            };
            let args = vec![Value::bits(
                width,
                vmlab_hv_tcg_ir::native::mask(width as u16),
            )];
            let (mut trace, expected) = Trace::record(
                &mut interpreter,
                &function,
                args.clone(),
                Options::default(),
            )
            .unwrap();
            assert!(matches!(expected, Value::Integer(_)));
            assert!(
                matches!(trace.run(&mut interpreter, &args, 1).unwrap(), Outcome::Returned(actual, 1) if actual == expected),
                "width {width}"
            );
        }
    }

    #[test]
    fn folded_pure_helpers_guard_configuration_and_keep_error_locations() {
        let mut code = (0..40)
            .map(|i| Op::Declare(format!("dead{i}"), Type::Bool, None))
            .collect::<Vec<_>>();
        code.extend([
            Op::Assign(Location::Id("return".into()), Expr::Id("mode".into())),
            Op::Return,
        ]);
        let helper = Function {
            name: "helper".into(),
            args: vec![],
            result: Type::Bool,
            locals: BTreeMap::new(),
            code,
        };
        let root = Function {
            name: "root".into(),
            args: vec![],
            result: Type::Bool,
            locals: BTreeMap::new(),
            code: vec![
                Op::Call(
                    Location::Id("return".into()),
                    Target::Function("helper".into()),
                    vec![],
                ),
                Op::Return,
            ],
        };
        let program = Program {
            functions: BTreeMap::from([("helper".into(), helper)]),
            globals: BTreeMap::from([("mode".into(), Type::Bool)]),
            ..Program::default()
        };
        let mut interpreter = Interpreter::new(&program, FlatMemory::new(0, vec![]));
        interpreter.initialize().unwrap();
        let (mut trace, _) =
            Trace::record(&mut interpreter, &root, vec![], Options::default()).unwrap();
        assert!(!interpreter.source_cache.contains_key("helper"));
        assert!(
            trace
                .recorded
                .block
                .context_guards
                .iter()
                .any(|guard| guard.name == "mode")
        );
        interpreter.globals.insert("mode".into(), Value::Bool(true));
        assert!(matches!(
            trace.run(&mut interpreter, &[], 1).unwrap(),
            Outcome::Miss
        ));

        let mut program = program;
        program.functions.get_mut("helper").unwrap().code.insert(
            40,
            Op::Call(
                Location::Id("assertion".into()),
                Target::Intrinsic("zsail_assert".into()),
                vec![Expr::Bool(false), Expr::String("failure".into())],
            ),
        );
        let mut interpreter = Interpreter::new(&program, FlatMemory::new(0, vec![]));
        interpreter.initialize().unwrap();
        let expected = interpreter
            .run_function(&root, vec![])
            .unwrap_err()
            .to_string();
        let actual = match Trace::record(&mut interpreter, &root, vec![], Options::default()) {
            Err(error) => error.to_string(),
            Ok(_) => panic!("assertion succeeded"),
        };
        assert_eq!(actual, expected);
        assert!(actual.contains("helper IR instruction 40"), "{actual}");
    }

    #[test]
    fn source_snapshots_keep_the_actual_root_definition() {
        let program = Program {
            globals: BTreeMap::from([("decision".into(), Type::Bool)]),
            ..Program::default()
        };
        let mut interpreter = Interpreter::new(&program, FlatMemory::new(0, vec![]));
        interpreter.initialize().unwrap();
        let mut root = Function {
            name: "root".into(),
            args: vec![],
            result: Type::Integer(None),
            locals: BTreeMap::new(),
            code: vec![
                Op::Assign(
                    Location::Id("return".into()),
                    Expr::Integer("7".into(), None),
                ),
                Op::Return,
            ],
        };
        Trace::record(&mut interpreter, &root, vec![], Options::default()).unwrap();
        root.code = vec![
            Op::Branch(Expr::Id("decision".into()), 3),
            Op::Assign(
                Location::Id("return".into()),
                Expr::Integer("11".into(), None),
            ),
            Op::Jump(4),
            Op::Assign(
                Location::Id("return".into()),
                Expr::Integer("9".into(), None),
            ),
            Op::Return,
        ];
        let options = Options {
            dynamic_globals: BTreeSet::from(["decision".into()]),
            ..Options::default()
        };
        let (mut trace, _) = Trace::record(&mut interpreter, &root, vec![], options).unwrap();
        interpreter
            .globals
            .insert("decision".into(), Value::Bool(true));
        assert!(
            matches!(trace.run(&mut interpreter, &[], 1).unwrap(), Outcome::Resumed(value, 1) if value == Value::int(9))
        );
    }

    #[test]
    fn native_memory_reads_preserve_full_width_data_without_replaying() {
        struct CountingMemory {
            ram: FlatMemory,
            value: u64,
            reads: usize,
        }
        impl Memory for CountingMemory {
            fn read(&mut self, address: u64, bytes: &mut [u8]) -> Result<()> {
                assert_eq!((address, bytes.len()), (8, 8));
                self.reads += 1;
                bytes.copy_from_slice(&self.value.to_le_bytes());
                Ok(())
            }
            fn write(&mut self, address: u64, bytes: &[u8]) -> Result<()> {
                self.ram.write(address, bytes)
            }
            fn generation(&self) -> u64 {
                self.ram.generation()
            }
            fn ram_bounds(&self, address: u64) -> Option<(u64, u64)> {
                self.ram.ram_bounds(address)
            }
            fn transaction_lock(&self) -> Option<std::sync::Arc<parking_lot::ReentrantMutex<()>>> {
                None
            }
            fn load_reserved(&mut self, address: u64, bytes: &mut [u8]) -> Result<u64> {
                self.ram.load_reserved(address, bytes)
            }
            fn store_conditional(
                &mut self,
                address: u64,
                bytes: &[u8],
                generation: u64,
            ) -> Result<bool> {
                self.ram.store_conditional(address, bytes, generation)
            }
        }
        let function = Function {
            name: "loaded_pointer".into(),
            args: vec![],
            result: Type::Bits(Some(64)),
            locals: BTreeMap::new(),
            code: vec![
                Op::Call(
                    Location::Id("loaded".into()),
                    Target::Memory {
                        name: "read_mem".into(),
                        write: false,
                        exclusive: false,
                        fetch: false,
                    },
                    vec![
                        Expr::Unit,
                        Expr::Unit,
                        Expr::Bits("0x0000000000000008".into()),
                        Expr::Integer("8".into(), None),
                    ],
                ),
                Op::Assign(Location::Id("return".into()), Expr::Id("loaded".into())),
                Op::Return,
            ],
        };
        let program = Program::default();
        let mut interpreter = Interpreter::new(
            &program,
            CountingMemory {
                ram: FlatMemory::new(0, vec![0; 256]),
                value: 16,
                reads: 0,
            },
        );
        let options = Options {
            narrow_ram_inputs: true,
            ..Options::default()
        };
        let (mut trace, first) =
            Trace::record(&mut interpreter, &function, vec![], options).unwrap();
        assert_eq!(first, Value::bits(64, 16u64));
        for value in [17, 32, 256, u64::MAX, 0, 8, 19, 512] {
            interpreter.memory.value = value;
            let reads = interpreter.memory.reads;
            let outcome = trace.run(&mut interpreter, &[], 1).unwrap();
            let (Outcome::Returned(actual, 1) | Outcome::Resumed(actual, 1)) = outcome else {
                panic!("loaded pointer trace missed");
            };
            assert_eq!(actual, Value::bits(64, value));
            assert_eq!(interpreter.memory.reads, reads + 1);
        }
        let operations = interpreter.operations;
        for value in [17, u64::MAX, 512, 32] {
            interpreter.memory.value = value;
            assert!(
                matches!(trace.run(&mut interpreter, &[], 1).unwrap(), Outcome::Returned(actual, 1) if actual == Value::bits(64, value))
            );
        }
        assert_eq!(interpreter.operations, operations);
    }

    #[test]
    fn nested_native_alternatives_preserve_a_completed_mmio_read() {
        struct CountingMemory {
            ram: FlatMemory,
            reads: u8,
        }
        impl Memory for CountingMemory {
            fn read(&mut self, address: u64, bytes: &mut [u8]) -> Result<()> {
                assert_eq!((address, bytes.len()), (8, 1));
                self.reads += 1;
                bytes[0] = self.reads;
                Ok(())
            }
            fn write(&mut self, address: u64, bytes: &[u8]) -> Result<()> {
                self.ram.write(address, bytes)
            }
            fn generation(&self) -> u64 {
                self.ram.generation()
            }
            fn load_reserved(&mut self, address: u64, bytes: &mut [u8]) -> Result<u64> {
                self.ram.load_reserved(address, bytes)
            }
            fn store_conditional(
                &mut self,
                address: u64,
                bytes: &[u8],
                generation: u64,
            ) -> Result<bool> {
                self.ram.store_conditional(address, bytes, generation)
            }
            fn transaction_lock(&self) -> Option<std::sync::Arc<parking_lot::ReentrantMutex<()>>> {
                None
            }
        }
        let add = |constant: &str| {
            Expr::Op(
                "add_bits".into(),
                None,
                vec![Expr::Id("loaded".into()), Expr::Bits(constant.into())],
            )
        };
        let function = Function {
            name: "nested_read".into(),
            args: vec![],
            result: Type::Bits(Some(8)),
            locals: BTreeMap::new(),
            code: vec![
                Op::Call(
                    Location::Id("loaded".into()),
                    Target::Memory {
                        name: "read_mem".into(),
                        write: false,
                        exclusive: false,
                        fetch: false,
                    },
                    vec![
                        Expr::Unit,
                        Expr::Unit,
                        Expr::Bits("0x0000000000000008".into()),
                        Expr::Integer("1".into(), None),
                    ],
                ),
                Op::Branch(Expr::Id("decision".into()), 4),
                Op::Assign(Location::Id("return".into()), Expr::Id("loaded".into())),
                Op::Jump(9),
                Op::Branch(Expr::Id("other".into()), 7),
                Op::Assign(Location::Id("return".into()), add("0x01")),
                Op::Jump(9),
                Op::Assign(Location::Id("return".into()), add("0x02")),
                Op::Jump(9),
                Op::Return,
            ],
        };
        let program = Program {
            globals: BTreeMap::from([
                ("decision".into(), Type::Bool),
                ("other".into(), Type::Bool),
            ]),
            ..Program::default()
        };
        let mut interpreter = Interpreter::new(
            &program,
            CountingMemory {
                ram: FlatMemory::new(0, vec![0; 16]),
                reads: 0,
            },
        );
        interpreter.initialize().unwrap();
        let options = Options {
            dynamic_globals: BTreeSet::from(["decision".into(), "other".into()]),
            ..Options::default()
        };
        let (mut trace, first) =
            Trace::record(&mut interpreter, &function, vec![], options).unwrap();
        assert_eq!(first, Value::bits(8, 1u8));
        interpreter
            .globals
            .insert("decision".into(), Value::Bool(true));
        assert!(
            matches!(trace.run(&mut interpreter, &[], 1).unwrap(), Outcome::Resumed(value, 1) if value == Value::bits(8, 3u8))
        );
        interpreter
            .globals
            .insert("other".into(), Value::Bool(true));
        assert!(
            matches!(trace.run(&mut interpreter, &[], 1).unwrap(), Outcome::Resumed(value, 1) if value == Value::bits(8, 5u8))
        );
        let operations = interpreter.operations;
        for other in [false, true, false, true] {
            interpreter
                .globals
                .insert("other".into(), Value::Bool(other));
            let expected = interpreter.memory.reads + 1 + if other { 2 } else { 1 };
            assert!(
                matches!(trace.run(&mut interpreter, &[], 1).unwrap(), Outcome::Returned(value, 1) if value == Value::bits(8, expected))
            );
        }
        assert_eq!(interpreter.memory.reads, 7);
        assert_eq!(interpreter.operations, operations);
    }

    #[test]
    fn alternate_fragments_bind_live_words_without_replaying_stores_or_sail() {
        let function = Function {
            name: "alternate".into(),
            args: vec![("n".into(), Type::Integer(None))],
            result: Type::Integer(None),
            locals: BTreeMap::new(),
            code: vec![
                store(),
                Op::Branch(Expr::Id("decision".into()), 4),
                Op::Assign(Location::Id("return".into()), Expr::Id("n".into())),
                Op::Jump(5),
                Op::Assign(
                    Location::Id("return".into()),
                    Expr::Op(
                        "add_int".into(),
                        None,
                        vec![Expr::Id("n".into()), Expr::Integer("1".into(), None)],
                    ),
                ),
                Op::Return,
            ],
        };
        let program = Program {
            globals: BTreeMap::from([("decision".into(), Type::Bool)]),
            ..Program::default()
        };
        let mut interpreter = Interpreter::new(&program, FlatMemory::new(0, vec![0; 16]));
        interpreter.initialize().unwrap();
        let options = Options {
            dynamic_globals: BTreeSet::from(["decision".into()]),
            ..Options::default()
        };
        let (mut trace, _) =
            Trace::record(&mut interpreter, &function, vec![Value::int(7)], options).unwrap();
        interpreter
            .globals
            .insert("decision".into(), Value::Bool(true));
        assert!(
            matches!(trace.run(&mut interpreter, &[Value::int(9)], 1).unwrap(), Outcome::Resumed(value, 1) if value == Value::int(10))
        );
        let operations = interpreter.operations;
        let generation = interpreter.memory.generation();
        for value in [17, 29, 100] {
            assert!(
                matches!(trace.run(&mut interpreter, &[Value::int(value)], 1).unwrap(), Outcome::Returned(actual, 1) if actual == Value::int(value + 1))
            );
        }
        assert_eq!(interpreter.operations, operations);
        assert_eq!(interpreter.memory.generation(), generation + 3);
        assert_eq!(interpreter.depth, 0);
    }

    fn store() -> Op {
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
                Expr::Bits("0x0000000000000008".into()),
                Expr::Integer("1".into(), None),
                Expr::Bits("0x2a".into()),
            ],
        )
    }

    #[test]
    fn trace_guard_resumes_nested_caller_after_completed_store() {
        let callee = Function {
            name: "callee".into(),
            args: vec![],
            result: Type::Integer(None),
            locals: BTreeMap::new(),
            code: vec![
                store(),
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
                Op::Call(
                    Location::Id("return".into()),
                    Target::Function("callee".into()),
                    vec![],
                ),
                Op::Return,
            ],
        };
        let program = Program {
            functions: BTreeMap::from([("callee".into(), callee)]),
            globals: BTreeMap::from([("decision".into(), Type::Bool)]),
            ..Program::default()
        };
        let mut interpreter = Interpreter::new(&program, FlatMemory::new(0, vec![0; 16]));
        interpreter.initialize().unwrap();
        let options = Options {
            dynamic_globals: BTreeSet::from(["decision".into()]),
            ..Options::default()
        };
        let (mut trace, first) = Trace::record(&mut interpreter, &caller, vec![], options).unwrap();
        assert_eq!(trace.stop_reason(), None);
        assert_eq!(first, Value::int(7));
        assert!(
            matches!(trace.run(&mut interpreter, &[], 1).unwrap(), Outcome::Returned(value, 1) if value == first)
        );
        let generation = interpreter.memory.generation();
        interpreter
            .globals
            .insert("decision".into(), Value::Bool(true));
        assert!(
            matches!(trace.run(&mut interpreter, &[], 1).unwrap(), Outcome::Resumed(value, 1) if value == Value::int(9))
        );
        assert_eq!(interpreter.memory.generation(), generation + 1);
        assert_eq!(interpreter.depth, 0);
    }

    #[test]
    fn context_change_after_a_completed_child_does_not_replay_child() {
        let child = Function {
            name: "child".into(),
            args: vec![],
            result: Type::Bool,
            locals: BTreeMap::new(),
            code: vec![
                store(),
                Op::Assign(Location::Id("return".into()), Expr::Bool(true)),
                Op::Return,
            ],
        };
        let root = Function {
            name: "root".into(),
            args: vec![],
            result: Type::Bool,
            locals: BTreeMap::new(),
            code: vec![
                Op::Call(
                    Location::Id("mode".into()),
                    Target::Function("child".into()),
                    vec![],
                ),
                Op::Assign(Location::Id("return".into()), Expr::Id("mode".into())),
                Op::Return,
            ],
        };
        let program = Program {
            functions: BTreeMap::from([("child".into(), child)]),
            globals: BTreeMap::from([("mode".into(), Type::Bool)]),
            ..Program::default()
        };
        let mut interpreter = Interpreter::new(&program, FlatMemory::new(0, vec![0; 16]));
        interpreter.initialize().unwrap();
        let (mut trace, first) =
            Trace::record(&mut interpreter, &root, vec![], Options::default()).unwrap();
        assert_eq!(first, Value::Bool(true));
        assert_eq!(
            trace.stop_reason(),
            Some("call changes specialized context")
        );
        interpreter
            .globals
            .insert("mode".into(), Value::Bool(false));
        let generation = interpreter.memory.generation();
        assert!(matches!(
            trace.run(&mut interpreter, &[], 1).unwrap(),
            Outcome::Resumed(Value::Bool(true), 1)
        ));
        assert_eq!(interpreter.memory.generation(), generation + 1);
    }
}
