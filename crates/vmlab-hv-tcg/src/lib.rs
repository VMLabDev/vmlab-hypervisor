//! Reference execution of the same specialised IR consumed by the trace compiler.
mod cpu;
mod intrinsics;
mod memory;
mod softfloat;
mod value;
pub use cpu::{Privilege, Riscv64};
pub use memory::{FlatMemory, Memory, SharedMemory};
use num_bigint::BigUint;
use std::collections::BTreeMap;
pub use value::Value;
use vmlab_hv_tcg_ir::{Expr, Function, Location, Op, Program, Target, Type};

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("type error: {0}")]
    Type(String),
    #[error("undefined value: {0}")]
    Undefined(String),
    #[error("unsupported intrinsic: {0}")]
    Intrinsic(String),
    #[error("Sail exception: {0}")]
    Exception(String),
    #[error("physical access fault at {0:#x}")]
    Memory(u64),
    #[error("execution limit exceeded")]
    Limit,
    #[error("{function} IR instruction {instruction}: {source}")]
    At {
        function: String,
        instruction: usize,
        source: Box<Error>,
    },
}

type Frame = BTreeMap<String, Value>;

pub struct Interpreter<'a, M> {
    pub program: &'a Program,
    pub globals: BTreeMap<String, Value>,
    pub memory: M,
    pub operations: u64,
    pub operation_limit: u64,
    pub diagnostics: std::collections::VecDeque<String>,
    pub terminal: Vec<u8>,
    depth: u32,
    reservation: Option<(u64, u64)>,
    fetch_address: Option<u64>,
    decode_cache: BTreeMap<(u64, u64, u32), Value>,
    instruction_cache: BTreeMap<String, std::sync::Arc<Function>>,
}

impl<'a, M: Memory> Interpreter<'a, M> {
    pub fn new(program: &'a Program, memory: M) -> Self {
        Self {
            program,
            globals: BTreeMap::new(),
            memory,
            operations: 0,
            operation_limit: 100_000_000,
            diagnostics: std::collections::VecDeque::new(),
            terminal: Vec::new(),
            depth: 0,
            reservation: None,
            fetch_address: None,
            decode_cache: BTreeMap::new(),
            instruction_cache: BTreeMap::new(),
        }
    }

    pub fn initialize(&mut self) -> Result<()> {
        for (name, ty) in &self.program.globals {
            self.globals
                .insert(name.clone(), self.default_value(ty, 0)?);
        }
        self.globals
            .insert("have_exception".into(), Value::Bool(false));
        self.globals
            .insert("current_exception".into(), Value::Undefined);
        self.globals
            .insert("throw_location".into(), Value::String(String::new()));
        for variants in self.program.enums.values() {
            for variant in variants {
                self.globals
                    .insert(variant.clone(), Value::Enum(variant.clone()));
            }
        }
        for function in &self.program.initializers {
            self.run_function(function, Vec::new())?;
        }
        Ok(())
    }

    pub fn default_value(&self, ty: &Type, depth: u32) -> Result<Value> {
        if depth > 64 {
            return Ok(Value::Undefined);
        }
        Ok(match ty {
            Type::Unit => Value::Unit,
            Type::Bool => Value::Bool(false),
            Type::Bit => Value::bits(1, 0u8),
            Type::Integer(_) => Value::int(0),
            Type::Bits(width) => Value::bits(width.unwrap_or(0), 0u8),
            Type::String => Value::String(String::new()),
            Type::Enum(name) => self
                .program
                .enums
                .get(name)
                .and_then(|v| v.first())
                .map_or(Value::Undefined, |v| Value::Enum(v.clone())),
            Type::Struct(name) => {
                let fields = self
                    .program
                    .structs
                    .get(name)
                    .ok_or_else(|| Error::Type(format!("unknown struct {name}")))?;
                Value::Struct(
                    fields
                        .iter()
                        .map(|(n, t)| Ok((n.clone(), self.default_value(t, depth + 1)?)))
                        .collect::<Result<_>>()?,
                )
            }
            Type::Vector(_) | Type::List(_) => Value::Vector(Vec::new()),
            Type::FixedVector(n, ty) if *n <= 1_048_576 => {
                Value::Vector(vec![self.default_value(ty, depth + 1)?; *n as usize])
            }
            _ => Value::Undefined,
        })
    }

    fn lookup<'b>(&'b self, frame: &'b Frame, name: &str) -> Result<&'b Value> {
        frame
            .get(name)
            .or_else(|| self.globals.get(name))
            .ok_or_else(|| Error::Undefined(name.into()))
    }

    fn eval(&mut self, frame: &Frame, expr: &Expr) -> Result<Value> {
        Ok(match expr {
            Expr::Id(name) => self.lookup(frame, name)?.clone(),
            Expr::Integer(n, _) => Value::int(
                n.parse::<num_bigint::BigInt>()
                    .map_err(|_| Error::Type("integer literal".into()))?,
            ),
            Expr::Bits(bits) => {
                let radix = if bits.as_bytes()[1] == b'x' { 16 } else { 2 };
                let width = (bits.len() - 2) as u32 * if radix == 16 { 4 } else { 1 };
                Value::bits(
                    width,
                    if width == 0 {
                        BigUint::ZERO
                    } else {
                        BigUint::parse_bytes(&bits.as_bytes()[2..], radix)
                            .ok_or_else(|| Error::Type("bitvector literal".into()))?
                    },
                )
            }
            Expr::Bool(b) => Value::Bool(*b),
            Expr::String(s) => Value::String(s.clone()),
            Expr::Unit => Value::Unit,
            Expr::Undefined(_) => Value::Undefined,
            Expr::Ref(name) => Value::Ref(name.clone()),
            Expr::Struct(_, fields) => Value::Struct(
                fields
                    .iter()
                    .map(|(n, e)| Ok((n.clone(), self.eval(frame, e)?)))
                    .collect::<Result<_>>()?,
            ),
            Expr::Field(e, field) => self.eval(frame, e)?.field(field)?.clone(),
            // Jib's `is` is a constructor mismatch test, used to skip a case.
            Expr::Is(e, tag) => Value::Bool(
                !matches!(self.eval(frame, e)?, Value::Union(ref actual, _) if actual == tag),
            ),
            Expr::As(e, tag) => match self.eval(frame, e)? {
                Value::Union(actual, value) if actual == *tag => *value,
                _ => return Err(Error::Type(format!("expected constructor {tag}"))),
            },
            Expr::Op(name, width, args) => {
                let args = args
                    .iter()
                    .map(|e| self.eval(frame, e))
                    .collect::<Result<Vec<_>>>()?;
                self.intrinsic(name, *width, &args)?
            }
        })
    }

    fn assign(&mut self, frame: &mut Frame, location: &Location, value: Value) -> Result<()> {
        fn path(location: &Location, fields: &mut Vec<String>) -> Option<String> {
            match location {
                Location::Id(name) => Some(name.clone()),
                Location::Field(base, field) => {
                    let name = path(base, fields)?;
                    fields.push(field.clone());
                    Some(name)
                }
                Location::Deref(_) => None,
            }
        }
        if let Location::Deref(base) = location {
            let mut fields = Vec::new();
            let name =
                path(base, &mut fields).ok_or_else(|| Error::Type("nested dereference".into()))?;
            let Value::Ref(target) = self.lookup(frame, &name)?.clone() else {
                return Err(Error::Type("expected reference".into()));
            };
            return self.assign(frame, &Location::Id(target), value);
        }
        let mut fields = Vec::new();
        let root = path(location, &mut fields)
            .ok_or_else(|| Error::Type("field through reference".into()))?;
        let entry = if frame.contains_key(&root) || !self.globals.contains_key(&root) {
            frame.entry(root).or_insert(Value::Undefined)
        } else {
            self.globals.get_mut(&root).expect("global exists")
        };
        let mut slot = entry;
        for field in fields {
            let Value::Struct(map) = slot else {
                return Err(Error::Type("assignment to non-struct field".into()));
            };
            slot = map
                .get_mut(&field)
                .ok_or_else(|| Error::Type(format!("missing field {field}")))?;
        }
        *slot = value;
        Ok(())
    }

    pub fn call(&mut self, name: &str, args: Vec<Value>) -> Result<Value> {
        if name == "zplat_term_write" {
            let byte = args
                .first()
                .ok_or_else(|| Error::Type("terminal argument".into()))?
                .u64()?;
            self.terminal
                .push(u8::try_from(byte).map_err(|_| Error::Type("terminal byte".into()))?);
            return Ok(Value::Unit);
        }
        if name.starts_with("zriscv_") {
            return softfloat::execute(self.program, name, &args);
        }
        let function = self
            .program
            .functions
            .get(name)
            .ok_or_else(|| Error::Undefined(format!("function {name}")))?;
        if name == "zexecute"
            && let Some(Value::Union(tag, _)) = args.first()
        {
            // AMO semantics contain separate generated reads and writes. Hold
            // the memory transaction across that sequence, including MMU work.
            let transaction = if matches!(tag.as_str(), "zAMO" | "zSSAMOSWAP") {
                self.memory.transaction_lock()
            } else {
                None
            };
            let _atomic = transaction.as_ref().map(|lock| lock.lock());
            let builder = self
                .instruction_cache
                .entry(tag.clone())
                .or_insert_with(|| {
                    let mut builder = function.clone();
                    vmlab_hv_tcg_ir::specialize_constructor(&mut builder, tag);
                    std::sync::Arc::new(builder)
                })
                .clone();
            return self.run_function(&builder, args);
        }
        if name == "zpt_walk" {
            // Serialize the complete PTE read/check/A-D update with RAM stores.
            let transaction = self.memory.transaction_lock();
            let _pte = transaction.as_ref().map(|lock| lock.lock());
            return self.run_function(function, args);
        }
        if matches!(name, "zext_decode" | "zext_decode_compressed")
            && let Some(address) = self.fetch_address
        {
            let key = (address, self.memory.generation(), args[0].u64()? as u32);
            if let Some(value) = self.decode_cache.get(&key) {
                return Ok(value.clone());
            }
            let value = self.run_function(function, args)?;
            if self.decode_cache.len() >= 4096 {
                self.decode_cache.clear();
            }
            self.decode_cache.insert(key, value.clone());
            return Ok(value);
        }
        self.run_function(function, args)
    }

    pub fn run_function(&mut self, function: &Function, args: Vec<Value>) -> Result<Value> {
        if self.depth >= 256 {
            return Err(Error::Limit);
        }
        if args.len() != function.args.len() {
            return Err(Error::Type(format!("{} argument count", function.name)));
        }
        let mut frame: Frame = function
            .args
            .iter()
            .map(|(n, _)| n.clone())
            .zip(args)
            .collect();
        frame.insert(
            "return".into(),
            if matches!(function.result, Type::Struct(_) | Type::List(_)) {
                self.default_value(&function.result, 0)?
            } else {
                Value::Undefined
            },
        );
        self.depth += 1;
        let result = self.run_frame(function, &mut frame);
        self.depth -= 1;
        result
    }

    fn run_frame(&mut self, function: &Function, frame: &mut Frame) -> Result<Value> {
        let mut pc = 0;
        while let Some(op) = function.code.get(pc) {
            self.operations += 1;
            if self.operations > self.operation_limit {
                return Err(Error::Limit);
            }
            let step = (|| -> Result<Option<usize>> {
                match op {
                    Op::Declare(name, ty, init) => {
                        // Jib aggregate declarations allocate their fields; a list starts nil.
                        let value = match init {
                            Some(e) => self.eval(frame, e)?,
                            None if matches!(ty, Type::List(_) | Type::Struct(_)) => {
                                self.default_value(ty, 0)?
                            }
                            None => Value::Undefined,
                        };
                        frame.insert(name.clone(), value);
                    }
                    Op::Assign(loc, expr) => {
                        let v = self.eval(frame, expr)?;
                        self.assign(frame, loc, v)?;
                    }
                    Op::Call(loc, target, args) => {
                        let args = args
                            .iter()
                            .map(|e| self.eval(frame, e))
                            .collect::<Result<Vec<_>>>()?;
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
                        self.assign(frame, loc, value)?;
                    }
                    Op::Branch(expr, target) => {
                        if self.eval(frame, expr)?.boolean()? {
                            return Ok(Some(*target));
                        }
                    }
                    Op::Jump(target) => return Ok(Some(*target)),
                    // Jib arbitrary is an undefined return value, commonly on
                    // the exceptional path. The caller must still get to test
                    // have_exception and run its generated catch code.
                    Op::Return | Op::Arbitrary => return Ok(None),
                    Op::Raise(cause) => return Err(Error::Exception(cause.clone())),
                    Op::Specialize(_, _) => {}
                }
                Ok(Some(pc + 1))
            })();
            match step {
                Ok(Some(next)) => pc = next,
                Ok(None) => return Ok(frame.remove("return").unwrap_or(Value::Unit)),
                Err(source) => {
                    return Err(Error::At {
                        function: function.name.clone(),
                        instruction: pc,
                        source: Box::new(source),
                    });
                }
            }
        }
        Err(Error::Type(format!("{} fell off IR", function.name)))
    }

    fn memory_operation(&mut self, args: &[Value], write: bool, exclusive: bool) -> Result<Value> {
        if args.len() != if write { 5 } else { 4 } {
            return Err(Error::Type("memory operation arity".into()));
        }
        let address = args[2].u64()?;
        let len = args[3].index()?;
        if len > 4096 {
            return Err(Error::Type("memory operation exceeds 4096 bytes".into()));
        }
        if write {
            let (_, bits) = args[4].bitvector()?;
            let mut bytes = bits.to_bytes_le();
            bytes.resize(len, 0);
            let result = if exclusive {
                match self.reservation.take() {
                    Some((reserved, generation)) if reserved == address => self
                        .memory
                        .store_conditional(address, &bytes[..len], generation)?,
                    _ => false,
                }
            } else {
                self.memory.write(address, &bytes[..len])?;
                true
            };
            self.reservation = None;
            Ok(Value::Bool(result))
        } else {
            let mut bytes = vec![0; len];
            if exclusive {
                let generation = self.memory.load_reserved(address, &mut bytes)?;
                self.reservation = Some((address, generation));
            } else {
                self.memory.read(address, &mut bytes)?;
            }
            Ok(Value::bits(len as u32 * 8, BigUint::from_bytes_le(&bytes)))
        }
    }
}
