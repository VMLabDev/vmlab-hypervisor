use crate::{Frame, Interpreter, Memory, Value};
use num_traits::ToPrimitive;
use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use vmlab_hv_tcg_ir::native::block::{
    Binding, Block, Checkpoint, ConstantId, ContextGuard, Exit, Recipe, SourceFrame, ValueId,
};
use vmlab_hv_tcg_ir::native::{Binary, Range, ScalarId, ScalarType};
use vmlab_hv_tcg_ir::{Expr, Function, Location, Op, Target, Type};

mod lower;

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum Projection {
    Field(String),
    Index(usize),
    Payload(String),
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum InputSource {
    Argument(usize),
    Global(String, Vec<Projection>),
    Local(usize, String, Vec<Projection>),
    FetchAddress,
}

#[derive(Clone, Debug)]
pub(crate) enum Shape {
    Scalar(ScalarType),
    Bits(u32),
    Struct(Vec<String>),
    Union(String),
    Vector(usize),
    Other(Value),
}

#[derive(Clone, Debug)]
pub(crate) struct Input {
    pub source: InputSource,
    pub shape: Shape,
    pub range: Option<Range>,
}

#[derive(Clone, Default)]
pub(crate) struct Options {
    pub dynamic_globals: BTreeSet<String>,
    pub pc: Option<u64>,
    pub step_argument: Option<usize>,
    pub narrow_ram_inputs: bool,
    pub stop_at_control_flow: bool,
    pub unbounded_globals: BTreeSet<String>,
    pub small_integer_globals: BTreeSet<String>,
}

pub(crate) struct Recorded {
    pub block: Block,
    pub constants: Vec<Value>,
    pub inputs: Vec<Input>,
    pub stop_reason: Option<String>,
    pub dynamic_pc: bool,
}

enum Pending {
    None,
    Value(ValueId),
    Call {
        args: Vec<ValueId>,
        location: Location,
    },
    Memory {
        args: Vec<ValueId>,
        error: usize,
        address: u64,
    },
}

struct ShadowFrame {
    function: Arc<Function>,
    source: Arc<Source>,
    locals: BTreeMap<String, ValueId>,
    pc: usize,
    call_site: usize,
    pending_result: Option<Location>,
    pending: Pending,
    memo_candidate: Option<MemoCandidate>,
}

struct MemoCandidate {
    args: Vec<Value>,
    versions: Vec<(usize, u64)>,
    identity: u64,
}

pub(crate) struct Source {
    function: Arc<Function>,
    liveness: std::sync::OnceLock<vmlab_hv_tcg_ir::native::liveness::Liveness>,
}

impl Source {
    pub fn matches(&self, function: &Function) -> bool {
        std::ptr::eq(self.function.as_ref(), function) || self.function.as_ref() == function
    }
    pub fn new(function: &Function) -> Self {
        Self::from_arc(Arc::new(function.clone()))
    }

    pub fn from_arc(function: Arc<Function>) -> Self {
        Self {
            function,
            liveness: std::sync::OnceLock::new(),
        }
    }
}

pub(crate) struct Recorder {
    block: Block,
    constants: Vec<Value>,
    constant_ids: BTreeMap<Value, ConstantId>,
    inputs: Vec<Input>,
    input_ids: BTreeMap<InputSource, usize>,
    scalar_values: std::collections::HashMap<(ScalarId, ScalarType), ValueId>,
    frames: Vec<ShadowFrame>,
    globals: BTreeMap<String, ValueId>,
    dirty: BTreeSet<String>,
    guards: BTreeSet<(ScalarId, bool)>,
    context: BTreeSet<String>,
    options: Options,
    returned: Option<ValueId>,
    completed_steps: u32,
    fetch_address: Option<ValueId>,
    stopped: Option<String>,
    folded_result: Option<crate::Result<Value>>,
}

impl Recorder {
    pub fn seed<M: Memory>(
        interpreter: &mut Interpreter<'_, M>,
        options: Options,
        frames: &[crate::continuation::RuntimeFrame],
        dynamic: &[BTreeSet<String>],
        known_globals: &Frame,
        fetch_address: Option<u64>,
    ) -> Self {
        let mut recorder = Self::new(options);
        if let Some(address) = fetch_address {
            recorder.fetch_address =
                Some(recorder.input(InputSource::FetchAddress, &Value::bits(64, address)));
        }
        for (name, value) in known_globals {
            let symbol = recorder.constant(value.clone());
            recorder.globals.insert(name.clone(), symbol);
        }
        for (index, frame) in frames.iter().enumerate() {
            let key = if index == 0 {
                format!("root:{}", frame.function.name)
            } else {
                frame.function.name.clone()
            };
            if interpreter
                .source_cache
                .get(&key)
                .is_some_and(|source| !source.matches(&frame.function))
            {
                interpreter.source_cache.remove(&key);
            }
            let source = interpreter
                .source_cache
                .entry(key)
                .or_insert_with(|| Arc::new(Source::from_arc(frame.function.clone())))
                .clone();
            let locals = frame
                .locals
                .iter()
                .map(|(name, value)| {
                    let symbol = if dynamic[index].contains(name) {
                        recorder.input(InputSource::Local(index, name.clone(), Vec::new()), value)
                    } else {
                        recorder.constant(value.clone())
                    };
                    (name.clone(), symbol)
                })
                .collect();
            recorder.frames.push(ShadowFrame {
                function: frame.function.clone(),
                source,
                locals,
                pc: frame.pc,
                call_site: frame.call_site,
                pending_result: frame.pending_result.clone(),
                pending: Pending::None,
                memo_candidate: None,
            });
        }
        recorder
    }

    pub fn resume_result<M: Memory>(
        &mut self,
        interpreter: &Interpreter<'_, M>,
        location: &Location,
    ) {
        if self.stopped.is_some() {
            return;
        }
        let value = self.returned.expect("recorded continuation child return");
        if self.assign_symbol(interpreter, location, value).is_none() {
            let exit = self.snapshot(None, Some(value));
            self.block
                .events
                .push(vmlab_hv_tcg_ir::native::block::Event::Exit(exit));
            self.stopped = Some("continuation changes specialized context".into());
        } else {
            self.frames.last_mut().expect("caller").pending_result = None;
        }
    }

    pub fn new(options: Options) -> Self {
        Self {
            block: Block::default(),
            constants: Vec::new(),
            constant_ids: BTreeMap::new(),
            inputs: Vec::new(),
            input_ids: BTreeMap::new(),
            scalar_values: std::collections::HashMap::new(),
            frames: Vec::new(),
            globals: BTreeMap::new(),
            dirty: BTreeSet::new(),
            guards: BTreeSet::new(),
            context: BTreeSet::new(),
            options,
            returned: None,
            completed_steps: 0,
            fetch_address: None,
            stopped: None,
            folded_result: None,
        }
    }

    fn constant(&mut self, value: Value) -> ValueId {
        if let Some(id) = self.constant_ids.get(&value) {
            return *id;
        }
        let constant = self.constants.len();
        self.constants.push(value.clone());
        let id = self.block.value(Recipe::Constant(constant));
        self.constant_ids.insert(value, id);
        id
    }

    fn known(&self, value: ValueId) -> Option<Value> {
        match &self.block.values[value] {
            Recipe::Constant(id) => Some(self.constants[*id].clone()),
            Recipe::Scalar { value, ty } => {
                let value = self.block.graph.literal(*value)?;
                Some(match ty {
                    ScalarType::Bool => Value::Bool(value != 0),
                    ScalarType::Bits(width) => Value::bits((*width).into(), value),
                    ScalarType::Integer => Value::int(value as i128),
                })
            }
            Recipe::Struct(fields) => Some(Value::Struct(
                fields
                    .iter()
                    .map(|(name, value)| Some((name.clone(), self.known(*value)?)))
                    .collect::<Option<_>>()?,
            )),
            Recipe::Union(tag, value) => {
                Some(Value::Union(tag.clone(), Box::new(self.known(*value)?)))
            }
            Recipe::Vector(values) => Some(Value::Vector(
                values
                    .iter()
                    .map(|v| self.known(*v))
                    .collect::<Option<_>>()?,
            )),
            _ => None,
        }
    }

    fn scalar(&mut self, value: ValueId) -> Option<(ScalarId, ScalarType)> {
        if let Recipe::Scalar { value, ty } = self.block.values[value] {
            return Some((self.block.graph.resolve(value), ty));
        }
        let (ty, raw) = match self.known(value)? {
            Value::Bool(value) => (ScalarType::Bool, u128::from(value)),
            Value::Bits { width, value } if width <= 128 => {
                (ScalarType::Bits(width as u16), value.to_u128()?)
            }
            Value::Integer(value) => (ScalarType::Integer, value.to_i128()? as u128),
            _ => return None,
        };
        Some((self.block.graph.constant(ty, raw), ty))
    }

    fn scalar_value(&mut self, scalar: ScalarId, ty: ScalarType) -> ValueId {
        if let Some(id) = self.scalar_values.get(&(scalar, ty)) {
            return *id;
        }
        let id = self.block.value(Recipe::Scalar { value: scalar, ty });
        self.scalar_values.insert((scalar, ty), id);
        id
    }

    fn input(&mut self, source: InputSource, value: &Value) -> ValueId {
        self.bounded_input(source, value, None)
    }

    fn bounded_input(
        &mut self,
        source: InputSource,
        value: &Value,
        bounds: Option<Range>,
    ) -> ValueId {
        if let Some(id) = self.input_ids.get(&source).copied() {
            return self.input_recipe(id, value);
        }
        let (shape, range) = match value {
            Value::Bool(_) => (Shape::Scalar(ScalarType::Bool), None),
            Value::Bits { width, .. } if *width <= 128 => {
                (Shape::Scalar(ScalarType::Bits(*width as u16)), None)
            }
            Value::Integer(value) if value.to_i128().is_some() => {
                let n = value.to_i128().unwrap();
                let range = if n >= 0 && n <= i128::from(u64::MAX) {
                    Range {
                        min: 0,
                        max: i128::from(u64::MAX),
                    }
                } else if i64::try_from(n).is_ok() {
                    Range {
                        min: i128::from(i64::MIN),
                        max: i128::from(i64::MAX),
                    }
                } else {
                    Range {
                        min: i128::MIN,
                        max: i128::MAX,
                    }
                };
                (Shape::Scalar(ScalarType::Integer), Some(range))
            }
            Value::Bits { width, .. } => (Shape::Bits(*width), None),
            Value::Struct(fields) => (Shape::Struct(fields.keys().cloned().collect()), None),
            Value::Union(tag, _) => (Shape::Union(tag.clone()), None),
            Value::Vector(values) => (Shape::Vector(values.len()), None),
            other => (Shape::Other(other.clone()), None),
        };
        let id = self.inputs.len();
        self.inputs.push(Input {
            source: source.clone(),
            shape,
            range: bounds.or(range),
        });
        self.input_ids.insert(source, id);
        self.input_recipe(id, value)
    }

    fn input_recipe(&mut self, id: usize, value: &Value) -> ValueId {
        if let Shape::Scalar(ty) = self.inputs[id].shape {
            let scalar = self.block.graph.input(id, ty, self.inputs[id].range);
            return self.scalar_value(scalar, ty);
        }
        let source = self.inputs[id].source.clone();
        let project = |projection| match &source {
            InputSource::Global(name, path) => {
                let mut path = path.clone();
                path.push(projection);
                InputSource::Global(name.clone(), path)
            }
            InputSource::Local(frame, name, path) => {
                let mut path = path.clone();
                path.push(projection);
                InputSource::Local(*frame, name.clone(), path)
            }
            InputSource::Argument(_) | InputSource::FetchAddress => source.clone(),
        };
        match value {
            Value::Struct(fields)
                if matches!(source, InputSource::Global(..) | InputSource::Local(..)) =>
            {
                let fields = fields
                    .iter()
                    .map(|(name, value)| {
                        (
                            name.clone(),
                            self.input(project(Projection::Field(name.clone())), value),
                        )
                    })
                    .collect();
                self.block.value(Recipe::Struct(fields))
            }
            Value::Union(tag, value)
                if matches!(source, InputSource::Global(..) | InputSource::Local(..)) =>
            {
                let value = self.input(project(Projection::Payload(tag.clone())), value);
                self.block.value(Recipe::Union(tag.clone(), value))
            }
            Value::Vector(values)
                if matches!(source, InputSource::Global(..) | InputSource::Local(..)) =>
            {
                let values = values
                    .iter()
                    .enumerate()
                    .map(|(i, value)| self.input(project(Projection::Index(i)), value))
                    .collect();
                self.block.value(Recipe::Vector(values))
            }
            _ if matches!(self.inputs[id].shape, Shape::Other(_)) => self.constant(value.clone()),
            _ => self.block.value(Recipe::Input(id)),
        }
    }

    fn global<M: Memory>(
        &mut self,
        interpreter: &Interpreter<'_, M>,
        name: &str,
    ) -> Option<ValueId> {
        if let Some(value) = self.globals.get(name) {
            return Some(*value);
        }
        let value = interpreter.globals.get(name)?.clone();
        let symbol = if self.options.dynamic_globals.contains(name) {
            let bounds = if self.options.narrow_ram_inputs
                && !self.options.unbounded_globals.contains(name)
                && matches!(value, Value::Bits { width: 64, .. })
            {
                let raw = value.u64().ok();
                raw.and_then(|address| interpreter.memory.ram_bounds(address))
                    .map(|(first, last)| Range {
                        min: i128::from(first),
                        max: i128::from(last),
                    })
                    .or_else(|| {
                        (self.options.small_integer_globals.contains(name)
                            && raw.is_some_and(|raw| raw <= u64::from(u16::MAX)))
                        .then_some(Range {
                            min: 0,
                            max: i128::from(u16::MAX),
                        })
                    })
            } else {
                None
            };
            self.bounded_input(InputSource::Global(name.into(), Vec::new()), &value, bounds)
        } else {
            let symbol = self.constant(value);
            if !(name == "zPC" && self.options.pc.is_some()) && self.context.insert(name.into()) {
                let Recipe::Constant(expected) = self.block.values[symbol] else {
                    unreachable!()
                };
                let slot = interpreter.globals.slot(name)?;
                self.block.context_guards.push(ContextGuard {
                    name: name.into(),
                    expected,
                    version: interpreter.globals.version(slot),
                });
            }
            symbol
        };
        self.globals.insert(name.into(), symbol);
        Some(symbol)
    }

    pub fn enter<M: Memory>(
        &mut self,
        interpreter: &Interpreter<'_, M>,
        source: Arc<Source>,
        actual: &Frame,
    ) {
        if self.stopped.is_some() {
            return;
        }
        let function = source.function.clone();
        let args = if let Some(parent) = self.frames.last_mut() {
            let Pending::Call { args, location, .. } = &parent.pending else {
                panic!("recorded callee without caller");
            };
            parent.pending_result = Some(location.clone());
            parent.call_site = parent.pc;
            parent.pc += 1;
            args.clone()
        } else {
            function
                .args
                .iter()
                .enumerate()
                .map(|(i, (name, _))| {
                    let mut value = self.input(InputSource::Argument(i), &actual[name]);
                    if self.options.step_argument == Some(i) && self.completed_steps != 0 {
                        let (input, _) = self.scalar(value).expect("step input is scalar");
                        let offset = self
                            .block
                            .graph
                            .constant(ScalarType::Integer, self.completed_steps.into());
                        let sum = self
                            .block
                            .graph
                            .binary(ScalarType::Integer, Binary::Add, input, offset)
                            .expect("bounded step offset");
                        value = self.scalar_value(sum, ScalarType::Integer);
                    }
                    value
                })
                .collect()
        };
        let memo_candidate = (!self.frames.is_empty())
            .then(|| self.memo_candidate(interpreter, &function, &args))
            .flatten();
        let mut locals: BTreeMap<_, _> = function
            .args
            .iter()
            .map(|(name, _)| name.clone())
            .zip(args)
            .collect();
        locals.insert("return".into(), self.constant(actual["return"].clone()));
        self.frames.push(ShadowFrame {
            function,
            source,
            locals,
            pc: 0,
            call_site: 0,
            pending_result: None,
            pending: Pending::None,
            memo_candidate,
        });
    }

    pub fn leave<M: Memory>(&mut self, interpreter: &mut Interpreter<'_, M>, success: bool) {
        if self.stopped.is_some() {
            return;
        }
        let frame = self.frames.pop().expect("recorded frame");
        self.returned = frame.locals.get("return").copied();
        if success
            && let Some(candidate) = frame.memo_candidate
            && candidate.identity == interpreter.globals.identity()
            && candidate
                .versions
                .iter()
                .all(|(slot, version)| interpreter.globals.version(*slot) == *version)
            && let Some(value) = self.returned.and_then(|value| self.known(value))
            && let Some(memo) = interpreter.memo.get_mut(&frame.function.name)
        {
            memo.insert(&interpreter.globals, candidate.args, value);
        }
    }

    fn memo_candidate<M: Memory>(
        &self,
        interpreter: &Interpreter<'_, M>,
        function: &Function,
        args: &[ValueId],
    ) -> Option<MemoCandidate> {
        if !interpreter.memoize {
            return None;
        }
        let (dependencies, pure) = interpreter.memo.get(&function.name)?.record_dependencies();
        if pure {
            return None;
        }
        let args = args
            .iter()
            .map(|value| self.known(*value))
            .collect::<Option<Vec<_>>>()?;
        let versions = dependencies
            .iter()
            .map(|name| {
                self.known(*self.globals.get(name)?)?;
                let slot = interpreter.globals.slot(name)?;
                Some((slot, interpreter.globals.version(slot)))
            })
            .collect::<Option<Vec<_>>>()?;
        if interpreter.program.functions.get(&function.name)? != function {
            return None;
        }
        Some(MemoCandidate {
            args,
            versions,
            identity: interpreter.globals.identity(),
        })
    }

    fn bindings(&self) -> Arc<Vec<Binding>> {
        self.dirty
            .iter()
            .map(|name| Binding {
                name: name.clone(),
                value: self.globals[name],
            })
            .collect::<Vec<_>>()
            .into()
    }

    fn snapshot(&mut self, next_pc: Option<usize>, returned: Option<ValueId>) -> usize {
        let last = self.frames.len() - 1;
        let frames: Vec<_> =
            self.frames
                .iter()
                .enumerate()
                .map(|(index, frame)| {
                    let pc = if index == last {
                        next_pc.unwrap_or(frame.pc)
                    } else {
                        frame.pc
                    };
                    let liveness = frame.source.liveness.get_or_init(|| {
                        vmlab_hv_tcg_ir::native::liveness::analyze(&frame.function)
                    });
                    let mut declared = vec![0u64; liveness.names.len().div_ceil(64)];
                    for name in frame.locals.keys() {
                        let slot = liveness.slots[name];
                        declared[slot / 64] |= 1 << (slot % 64);
                    }
                    SourceFrame {
                        function: frame.function.clone(),
                        locals: frame
                            .locals
                            .iter()
                            .filter(|(name, _)| {
                                let slot = liveness.slots[*name];
                                liveness
                                    .live
                                    .get(pc)
                                    .is_some_and(|live| live[slot / 64] & (1 << (slot % 64)) != 0)
                            })
                            .map(|(name, value)| Binding {
                                name: name.clone(),
                                value: *value,
                            })
                            .collect::<Vec<_>>()
                            .into(),
                        local_names: liveness.names.clone(),
                        declared,
                        pc,
                        pending_result: frame.pending_result.clone(),
                        call_site: frame.call_site,
                    }
                })
                .collect();
        self.block.exit(Exit {
            frames,
            globals: self.bindings(),
            completed_steps: self.completed_steps,
            returned,
            fetch_address: self.fetch_address,
        })
    }

    fn stop(&mut self, reason: impl Into<String>) {
        if self.stopped.is_none() {
            let exit = self.snapshot(None, None);
            self.block
                .events
                .push(vmlab_hv_tcg_ir::native::block::Event::Exit(exit));
            self.stopped = Some(reason.into());
        }
    }

    fn expr<M: Memory>(
        &mut self,
        interpreter: &mut Interpreter<'_, M>,
        actual: &Frame,
        expr: &Expr,
    ) -> Option<ValueId> {
        match expr {
            Expr::Id(name) => self
                .frames
                .last()?
                .locals
                .get(name)
                .copied()
                .or_else(|| self.global(interpreter, name)),
            Expr::Struct(_, fields) => {
                let fields = fields
                    .iter()
                    .map(|(name, e)| Some((name.clone(), self.expr(interpreter, actual, e)?)))
                    .collect::<Option<_>>()?;
                Some(self.block.value(Recipe::Struct(fields)))
            }
            Expr::Field(e, name) => {
                let value = self.expr(interpreter, actual, e)?;
                match &self.block.values[value] {
                    Recipe::Struct(fields) => fields
                        .iter()
                        .find(|(field, _)| field == name)
                        .map(|(_, value)| *value),
                    _ => Some(self.constant(self.known(value)?.field(name).ok()?.clone())),
                }
            }
            Expr::As(e, tag) | Expr::Is(e, tag) => {
                let value = self.expr(interpreter, actual, e)?;
                let (actual_tag, payload) = match self.block.values[value].clone() {
                    Recipe::Union(tag, payload) => (tag, payload),
                    _ => {
                        let Value::Union(tag, payload) = self.known(value)? else {
                            return None;
                        };
                        (tag, self.constant(*payload))
                    }
                };
                if matches!(expr, Expr::Is(..)) {
                    Some(self.constant(Value::Bool(actual_tag != *tag)))
                } else if actual_tag == *tag {
                    Some(payload)
                } else {
                    None
                }
            }
            Expr::Op(name, width, args) => {
                let args = args
                    .iter()
                    .map(|e| self.expr(interpreter, actual, e))
                    .collect::<Option<_>>()?;
                self.intrinsic_value(interpreter, name, *width, args)
            }
            _ => Some(self.constant(interpreter.eval(actual, expr).ok()?)),
        }
    }

    fn update_fields(&mut self, root: ValueId, path: &[String], value: ValueId) -> Option<ValueId> {
        if path.is_empty() {
            return Some(value);
        }
        let mut fields = match self.block.values[root].clone() {
            Recipe::Struct(fields) => fields,
            _ => {
                let Value::Struct(fields) = self.known(root)? else {
                    return None;
                };
                fields
                    .into_iter()
                    .map(|(name, value)| (name, self.constant(value)))
                    .collect()
            }
        };
        let field = fields.iter_mut().find(|(name, _)| *name == path[0])?;
        field.1 = self.update_fields(field.1, &path[1..], value)?;
        Some(self.block.value(Recipe::Struct(fields)))
    }

    fn assign_symbol<M: Memory>(
        &mut self,
        interpreter: &Interpreter<'_, M>,
        location: &Location,
        value: ValueId,
    ) -> Option<()> {
        fn path(location: &Location, fields: &mut Vec<String>) -> Option<String> {
            match location {
                Location::Id(name) => Some(name.clone()),
                Location::Field(base, name) => {
                    let root = path(base, fields)?;
                    fields.push(name.clone());
                    Some(root)
                }
                _ => None,
            }
        }
        if let Location::Deref(base) = location {
            let Location::Id(name) = base.as_ref() else {
                return None;
            };
            let symbol = self
                .frames
                .last()?
                .locals
                .get(name)
                .copied()
                .or_else(|| self.global(interpreter, name))?;
            let Value::Ref(name) = self.known(symbol)? else {
                return None;
            };
            return self.assign_symbol(interpreter, &Location::Id(name), value);
        }
        let mut fields = Vec::new();
        let root = path(location, &mut fields)?;
        let local = self.frames.last()?.locals.contains_key(&root)
            || !interpreter.globals.contains_key(&root);
        let value = if fields.is_empty() {
            value
        } else {
            let original = if local {
                *self.frames.last()?.locals.get(&root)?
            } else {
                self.global(interpreter, &root)?
            };
            self.update_fields(original, &fields, value)?
        };
        if local {
            self.frames.last_mut()?.locals.insert(root, value);
        } else {
            if !self.options.dynamic_globals.contains(&root) && root != "zPC" {
                let old = self.global(interpreter, &root)?;
                if self.known(old)? != self.known(value)? {
                    return None;
                }
                // A guarded idempotent write has no value to commit.
                return Some(());
            }
            self.globals.insert(root.clone(), value);
            self.dirty.insert(root);
        }
        Some(())
    }

    fn prime_destination<M: Memory>(
        &mut self,
        interpreter: &Interpreter<'_, M>,
        location: &Location,
    ) -> Option<()> {
        match location {
            Location::Id(name) => {
                if !self.frames.last()?.locals.contains_key(name)
                    && interpreter.globals.contains_key(name)
                {
                    self.global(interpreter, name)?;
                }
            }
            Location::Field(base, _) => self.prime_destination(interpreter, base)?,
            Location::Deref(base) => {
                let Location::Id(name) = base.as_ref() else {
                    return None;
                };
                let symbol = self
                    .frames
                    .last()?
                    .locals
                    .get(name)
                    .copied()
                    .or_else(|| self.global(interpreter, name))?;
                let Value::Ref(name) = self.known(symbol)? else {
                    return None;
                };
                self.prime_destination(interpreter, &Location::Id(name))?;
            }
        }
        Some(())
    }

    pub fn before<M: Memory>(
        &mut self,
        interpreter: &mut Interpreter<'_, M>,
        actual: &Frame,
        op: &Op,
        pc: usize,
    ) {
        if self.stopped.is_some() {
            return;
        }
        self.frames.last_mut().expect("recorded frame").pc = pc;
        if self.block.values.len() > 250_000 {
            self.stop("recording value budget");
            return;
        }
        let result = (|| -> Option<()> {
            match op {
                Op::Declare(_, ty, expr) => {
                    let value = if let Some(expr) = expr {
                        self.expr(interpreter, actual, expr)?
                    } else {
                        self.constant(if matches!(ty, Type::List(_) | Type::Struct(_)) {
                            interpreter.default_value(ty, 0).ok()?
                        } else {
                            Value::Undefined
                        })
                    };
                    self.frames.last_mut()?.pending = Pending::Value(value);
                }
                Op::Assign(location, expr) => {
                    let value = self.expr(interpreter, actual, expr)?;
                    self.assign_symbol(interpreter, location, value)?;
                }
                Op::Call(location, target, exprs) => {
                    self.prime_destination(interpreter, location)?;
                    let args: Vec<_> = exprs
                        .iter()
                        .map(|e| self.expr(interpreter, actual, e))
                        .collect::<Option<_>>()?;
                    if self.options.narrow_ram_inputs
                        && let Target::Function(name) = target
                        && let Some(indices) = interpreter.address_arguments.get(name)
                    {
                        for &index in indices {
                            if let (Some(expr), Some(value)) = (exprs.get(index), args.get(index)) {
                                self.hint_address(interpreter, actual, expr, *value, pc);
                            }
                        }
                    }
                    let value = match target {
                        Target::Function(name)
                            if interpreter.program.functions.contains_key(name)
                                && !name.starts_with("zriscv_") =>
                        {
                            if let Some(result) = self.fold_function(interpreter, name, &args) {
                                match result {
                                    Ok(actual) => {
                                        let value = self.constant(actual.clone());
                                        self.folded_result = Some(Ok(actual));
                                        self.assign_symbol(interpreter, location, value)?;
                                        self.frames.last_mut()?.pending = Pending::Value(value);
                                    }
                                    Err(error) => self.folded_result = Some(Err(error)),
                                }
                                return Some(());
                            }
                            self.frames.last_mut()?.pending = Pending::Call {
                                args,
                                location: location.clone(),
                            };
                            return Some(());
                        }
                        Target::Intrinsic(name)
                            if vmlab_hv_tcg_ir::pure_intrinsic(name)
                                || matches!(
                                    name.as_str(),
                                    "instr_announce" | "branch_announce" | "event_callback"
                                ) =>
                        {
                            self.intrinsic_value(interpreter, name, None, args)?
                        }
                        Target::Native { operation, width } => {
                            self.native_value(operation, *width, &args)?
                        }
                        Target::Constructor(tag) if args.len() == 1 => {
                            self.block.value(Recipe::Union(tag.clone(), args[0]))
                        }
                        Target::Memory {
                            exclusive: false, ..
                        } => {
                            let len = self.known(args[3])?.index().ok()?;
                            if len == 0 || len > 16 {
                                return None;
                            }
                            self.scalar(args[2])?;
                            if args.len() == 5 {
                                self.scalar(args[4])?;
                            }
                            let error = self.snapshot(None, None);
                            let address = interpreter.eval(actual, &exprs[2]).ok()?.u64().ok()?;
                            self.frames.last_mut()?.pending = Pending::Memory {
                                args,
                                error,
                                address,
                            };
                            return Some(());
                        }
                        _ => return None,
                    };
                    // A pure primitive has not run yet, so this exit precedes it.
                    self.assign_symbol(interpreter, location, value)?;
                    self.frames.last_mut()?.pending = Pending::Value(value);
                }
                Op::Branch(expr, target) => {
                    let condition = self.expr(interpreter, actual, expr)?;
                    if self.known(condition).is_none() {
                        let (condition, ScalarType::Bool) = self.scalar(condition)? else {
                            return None;
                        };
                        let expected = interpreter.eval(actual, expr).ok()?.boolean().ok()?;
                        if self.guards.insert((condition, expected)) {
                            let exit =
                                self.snapshot(Some(if expected { pc + 1 } else { *target }), None);
                            self.block
                                .events
                                .push(vmlab_hv_tcg_ir::native::block::Event::Guard {
                                    condition,
                                    expected,
                                    exit,
                                });
                            self.block.graph.assume_guard(condition, expected);
                        }
                    }
                }
                Op::Raise(_) => return None,
                _ => {}
            }
            Some(())
        })();
        if result.is_none() {
            self.stop(format!(
                "{}:{pc}: {op:?}",
                self.frames.last().expect("frame").function.name
            ));
        }
    }

    pub fn after<M: Memory>(
        &mut self,
        interpreter: &mut Interpreter<'_, M>,
        actual: &Frame,
        op: &Op,
        pc: usize,
    ) {
        if self.stopped.is_some() {
            return;
        }
        let pending = std::mem::replace(
            &mut self.frames.last_mut().expect("frame").pending,
            Pending::None,
        );
        match (op, pending) {
            (Op::Declare(name, _, _), Pending::Value(value)) => {
                self.frames
                    .last_mut()
                    .unwrap()
                    .locals
                    .insert(name.clone(), value);
            }
            (Op::Call(location, _, _), Pending::Call { .. }) => {
                let value = self.returned.expect("callee return");
                if self.assign_symbol(interpreter, location, value).is_none() {
                    let exit = self.snapshot(Some(pc + 1), Some(value));
                    let frame = self.block.exits[exit].frames.last_mut().unwrap();
                    frame.pending_result = Some(location.clone());
                    frame.call_site = pc;
                    self.block
                        .events
                        .push(vmlab_hv_tcg_ir::native::block::Event::Exit(exit));
                    self.stopped = Some("call changes specialized context".into());
                }
                self.frames.last_mut().unwrap().pending_result = None;
            }
            (
                Op::Call(location, Target::Memory { write, fetch, .. }, _),
                Pending::Memory {
                    args,
                    error,
                    address: actual_address,
                },
            ) => {
                use vmlab_hv_tcg_ir::native::block::{CodeGuard, Event};
                let (address, _) = self.scalar(args[2]).unwrap();
                let bytes = self.known(args[3]).unwrap().index().unwrap() as u8;
                if *fetch && self.fetch_address.is_none() {
                    self.fetch_address = Some(args[2]);
                }
                let value = if *write {
                    let (data, _) = self.scalar(args[4]).unwrap();
                    let value = self.constant(Value::Bool(true));
                    self.assign_symbol(interpreter, location, value)
                        .expect("memory destination");
                    let invalidated = self.snapshot(Some(pc + 1), None);
                    self.block.events.push(Event::WriteMemory {
                        address,
                        value: data,
                        bytes,
                        exclusive: false,
                        result: None,
                        error,
                        invalidated,
                    });
                    value
                } else if *fetch
                    && let Some(stamp) = interpreter
                        .memory
                        .code_stamp(actual_address, bytes as usize)
                {
                    let expected = self
                        .block
                        .graph
                        .constant(self.block.graph.scalars[address].ty, actual_address.into());
                    let condition = self
                        .block
                        .graph
                        .binary(ScalarType::Bool, Binary::Equal, address, expected)
                        .unwrap();
                    if self.block.graph.literal(condition) != Some(1) {
                        self.block.events.push(Event::Guard {
                            condition,
                            expected: true,
                            exit: error,
                        });
                    }
                    self.block.code_guards.push(CodeGuard {
                        address: actual_address,
                        len: bytes as usize,
                        generation: stamp.generation,
                        identity: stamp.identity,
                        mapping: stamp.mapping,
                        first_page: stamp.first_page,
                        last_page: stamp.last_page,
                    });
                    self.constant(
                        interpreter
                            .lookup(
                                actual,
                                match location {
                                    Location::Id(name) => name,
                                    _ => panic!("fetch destination"),
                                },
                            )
                            .unwrap()
                            .clone(),
                    )
                } else {
                    let ty = ScalarType::Bits(u16::from(bytes) * 8);
                    let result = self.block.graph.effect(self.block.events.len(), ty);
                    self.block.events.push(Event::ReadMemory {
                        address,
                        bytes,
                        exclusive: false,
                        result,
                        error,
                    });
                    self.scalar_value(result, ty)
                };
                self.assign_symbol(interpreter, location, value)
                    .expect("memory destination");
            }
            _ => {}
        }
    }

    fn hint_address<M: Memory>(
        &mut self,
        interpreter: &Interpreter<'_, M>,
        actual: &Frame,
        expr: &Expr,
        value: ValueId,
        pc: usize,
    ) {
        let Expr::Id(name) = expr else {
            return;
        };
        let Some(observed) = interpreter
            .lookup(actual, name)
            .ok()
            .and_then(|value| value.u64().ok())
        else {
            return;
        };
        let Some((low, high)) = interpreter.memory.ram_bounds(observed) else {
            return;
        };
        let Some((value, ty)) = self.scalar(value) else {
            return;
        };
        let value = self.block.graph.resolve(value);
        if self.block.graph.scalars[value]
            .range
            .is_some_and(|range| range.min >= i128::from(low) && range.max <= i128::from(high))
        {
            return;
        }
        let exit = self.snapshot(Some(pc), None);
        if let Some(condition) = self
            .block
            .graph
            .unsigned_range(value, low.into(), high.into())
        {
            self.block
                .events
                .push(vmlab_hv_tcg_ir::native::block::Event::Guard {
                    condition,
                    expected: true,
                    exit,
                });
            self.block.graph.assume_guard(condition, true);
            return;
        }
        for (operation, bound, expected) in [
            (Binary::LessUnsigned, low, false),
            (Binary::LessEqualUnsigned, high, true),
        ] {
            let bound = self.block.graph.constant(ty, bound.into());
            let Some(condition) =
                self.block
                    .graph
                    .binary(ScalarType::Bool, operation, value, bound)
            else {
                return;
            };
            if self.block.graph.literal(condition) == Some(u128::from(expected)) {
                continue;
            }
            self.block
                .events
                .push(vmlab_hv_tcg_ir::native::block::Event::Guard {
                    condition,
                    expected,
                    exit,
                });
            self.block.graph.assume_guard(condition, expected);
        }
    }

    pub fn end_step<M: Memory>(&mut self, interpreter: &Interpreter<'_, M>) {
        if self.stopped.is_some() {
            return;
        }
        self.completed_steps += 1;
        self.block.checkpoint(Checkpoint {
            globals: self.bindings(),
            waiting: self.returned.expect("step return"),
            completed_steps: self.completed_steps,
            fetch_address: self.fetch_address,
        });
        self.fetch_address = None;
        for guard in &mut self.block.context_guards {
            if interpreter.globals.get(&guard.name) == Some(&self.constants[guard.expected]) {
                guard.version = interpreter
                    .globals
                    .version(interpreter.globals.slot(&guard.name).expect("context slot"));
            }
        }
    }

    pub fn finish(mut self) -> Recorded {
        let dynamic_pc = self.dynamic_pc();
        let mut seen = BTreeSet::new();
        self.block.code_guards.retain(|guard| {
            seen.insert((
                guard.generation,
                guard.identity,
                guard.mapping,
                guard.first_page,
                guard.last_page,
            ))
        });
        let constants = vmlab_hv_tcg_ir::native::dce::prune(&mut self.block);
        self.constants = constants
            .into_iter()
            .map(|id| self.constants[id].clone())
            .collect();
        Recorded {
            block: self.block,
            constants: self.constants,
            inputs: self.inputs,
            stop_reason: self.stopped,
            dynamic_pc,
        }
    }

    pub fn dynamic_pc(&self) -> bool {
        self.options.pc.is_some()
            && self
                .globals
                .get("zPC")
                .and_then(|value| self.known(*value))
                .is_none()
    }

    pub fn stopped(&self) -> bool {
        self.stopped.is_some()
    }

    pub fn take_folded_result(&mut self) -> Option<crate::Result<Value>> {
        self.folded_result.take()
    }

    fn fold_function<M: Memory>(
        &mut self,
        interpreter: &mut Interpreter<'_, M>,
        name: &str,
        args: &[ValueId],
    ) -> Option<crate::Result<Value>> {
        let (dependencies, pure) = interpreter.memo.get(name)?.record_dependencies();
        for dependency in dependencies {
            if let Some(symbol) = self.globals.get(dependency) {
                self.known(*symbol)?;
            } else if self.options.dynamic_globals.contains(dependency) {
                return None;
            }
        }
        let args = args
            .iter()
            .map(|id| self.known(*id))
            .collect::<Option<Vec<_>>>()?;
        for dependency in dependencies {
            let value = self.global(interpreter, dependency)?;
            self.known(value)?;
        }
        if pure {
            // The recording hook holds the recorder outside Interpreter.
            return Some(interpreter.call(name, args));
        }
        if !interpreter.memoize {
            return None;
        }
        // A hit proves every possible write left the guarded state unchanged.
        // A miss executes normally; the source frame snapshots its versions.
        let value = interpreter
            .memo
            .get_mut(name)?
            .lookup(&mut interpreter.globals, &args)?;
        interpreter.operations += 1;
        Some(if interpreter.operations > interpreter.operation_limit {
            Err(crate::Error::Limit)
        } else {
            Ok(value)
        })
    }
}
