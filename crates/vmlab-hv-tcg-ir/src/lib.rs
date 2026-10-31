//! Target-independent emulation IR. Branches address instruction indices.
mod address;
mod effects;
pub use address::address_arguments;
pub mod indexed;
pub mod native;
pub use effects::{Effects, effects, pure_intrinsic};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use vmlab_hv_sail_ir::{Definition, Instruction, Statement};
pub use vmlab_hv_sail_ir::{Expr, Location, Type};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Storage {
    Unit,
    Integer {
        host_bits: u32,
        value_bits: u32,
        signed: bool,
    },
    BigInteger,
    DynamicBits,
    Aggregate,
}

pub fn storage(ty: &Type) -> Storage {
    match ty {
        Type::Unit => Storage::Unit,
        Type::Bool | Type::Bit => Storage::Integer {
            host_bits: 8,
            value_bits: 1,
            signed: false,
        },
        Type::Integer(Some(width)) | Type::Bits(Some(width)) if *width <= 128 => {
            let host_bits = [8, 16, 32, 64, 128]
                .into_iter()
                .find(|n| n >= width)
                .unwrap();
            Storage::Integer {
                host_bits,
                value_bits: *width,
                signed: matches!(ty, Type::Integer(_)),
            }
        }
        Type::Integer(_) => Storage::BigInteger,
        Type::Bits(_) => Storage::DynamicBits,
        _ => Storage::Aggregate,
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum NativeOperation {
    Add,
    Sub,
    Not,
    And,
    Or,
    Xor,
    ShiftLeft,
    ShiftRight,
    ArithmeticShiftRight,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Target {
    Function(String),
    Intrinsic(String),
    Constructor(String),
    /// A fixed-width bitvector operation lowered to a host integer operation.
    Native {
        operation: NativeOperation,
        width: u32,
    },
    Memory {
        name: String,
        write: bool,
        exclusive: bool,
        fetch: bool,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Op {
    Declare(String, Type, Option<Expr>),
    Assign(Location, Expr),
    Call(Location, Target, Vec<Expr>),
    Branch(Expr, usize),
    Jump(usize),
    Return,
    Raise(String),
    Arbitrary,
    Specialize(String, Type),
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Function {
    pub name: String,
    pub args: Vec<(String, Type)>,
    pub result: Type,
    pub locals: BTreeMap<String, Type>,
    pub code: Vec<Op>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Register {
    pub name: String,
    pub ty: Type,
    pub storage: Storage,
    /// Index in the architectural register array. Stable in declaration order.
    pub index: usize,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Program {
    pub functions: BTreeMap<String, Function>,
    pub registers: Vec<Register>,
    pub globals: BTreeMap<String, Type>,
    pub enums: BTreeMap<String, Vec<String>>,
    pub structs: BTreeMap<String, Vec<(String, Type)>>,
    pub constructors: BTreeMap<String, Type>,
    pub declarations: BTreeMap<String, (Vec<Type>, Type)>,
    pub initializers: Vec<Function>,
}

#[derive(Debug, thiserror::Error)]
#[error("IR: {0}")]
pub struct Error(pub String);

pub fn lower(definitions: &[Definition]) -> Result<Program, Error> {
    let mut program = Program::default();
    let mut signatures = BTreeMap::new();
    let mut externs = BTreeMap::new();
    for d in definitions {
        match d {
            Definition::Signature {
                name,
                external,
                args,
                result,
                ..
            } => {
                signatures.insert(name.clone(), (args.clone(), result.clone()));
                if let Some(external) = external {
                    externs.insert(name.clone(), external.clone());
                }
            }
            Definition::Enum(name, members) => {
                program.enums.insert(name.clone(), members.clone());
            }
            Definition::Struct(name, fields) => {
                program.structs.insert(name.clone(), fields.clone());
            }
            Definition::Union(_, fields) => {
                program.constructors.extend(fields.iter().cloned());
            }
            Definition::Register(name, ty, _) => {
                program.registers.push(Register {
                    name: name.clone(),
                    ty: ty.clone(),
                    storage: storage(ty),
                    index: program.registers.len(),
                });
                program.globals.insert(name.clone(), ty.clone());
            }
            Definition::Let(fields, _) => {
                program.globals.extend(fields.iter().cloned());
            }
            _ => {}
        }
    }
    let target = |name: &str, external: bool| {
        if program.constructors.contains_key(name) {
            return Target::Constructor(name.into());
        }
        if let Some(ext) = externs.get(name) {
            if ext.starts_with("read_mem") || ext.starts_with("write_mem") {
                return Target::Memory {
                    name: ext.clone(),
                    write: ext.starts_with("write"),
                    exclusive: ext.contains("exclusive"),
                    fetch: ext.contains("ifetch"),
                };
            }
            return Target::Intrinsic(ext.clone());
        }
        if external {
            Target::Intrinsic(name.to_owned())
        } else {
            Target::Function(name.to_owned())
        }
    };
    let build = |name: String,
                 args: Vec<String>,
                 body: &[Instruction],
                 sig: (Vec<Type>, Type)|
     -> Result<Function, Error> {
        if args.len() != sig.0.len() {
            return Err(Error(format!(
                "{name}: signature has {} args, function has {}",
                sig.0.len(),
                args.len()
            )));
        }
        let mut locals = BTreeMap::new();
        let mut code = Vec::new();
        for instruction in body {
            code.push(match &instruction.statement {
                Statement::Declare(n, t, v) => {
                    locals.insert(n.clone(), t.clone());
                    Op::Declare(n.clone(), t.clone(), v.clone())
                }
                Statement::Copy(l, e) => Op::Assign(l.clone(), e.clone()),
                Statement::Call(l, ext, n, args) => {
                    Op::Call(l.clone(), target(n, *ext), args.clone())
                }
                Statement::Jump(e, n) => Op::Branch(e.clone(), *n),
                Statement::Goto(n) => Op::Jump(*n),
                Statement::End => Op::Return,
                Statement::Exit(cause) => Op::Raise(cause.clone()),
                Statement::Arbitrary => Op::Arbitrary,
                Statement::Monomorphize(n, t) => Op::Specialize(n.clone(), t.clone()),
            });
        }
        let mut function = Function {
            name,
            args: args.into_iter().zip(sig.0).collect(),
            result: sig.1,
            locals,
            code,
        };
        specialize(&mut function);
        Ok(function)
    };
    let mut functions = BTreeMap::new();
    let mut initializers = Vec::new();
    for d in definitions {
        match d {
            Definition::Function(name, args, body) => {
                let sig = signatures
                    .get(name)
                    .ok_or_else(|| Error(format!("missing signature for {name}")))?
                    .clone();
                functions.insert(name.clone(), build(name.clone(), args.clone(), body, sig)?);
            }
            Definition::Let(_, body) | Definition::Register(_, _, body) if !body.is_empty() => {
                // Jib initializers fall through implicitly; ordinary functions do not.
                let mut body = body.clone();
                body.push(Instruction {
                    statement: Statement::End,
                    source: Vec::new(),
                });
                initializers.push(build(
                    format!("initializer_{}", initializers.len()),
                    Vec::new(),
                    &body,
                    (Vec::new(), Type::Unit),
                )?);
            }
            _ => {}
        }
    }
    program.functions = functions;
    program.initializers = initializers;
    program.declarations = signatures;
    let declarations = &program.declarations;
    let globals = &program.globals;
    for function in program
        .functions
        .values_mut()
        .chain(program.initializers.iter_mut())
    {
        lower_bitvectors(function, declarations, globals);
    }
    Ok(program)
}

fn expression_type(
    expr: &Expr,
    types: &BTreeMap<String, Type>,
    base: &BTreeMap<String, Type>,
) -> Option<Type> {
    match expr {
        Expr::Id(name) => types.get(name).or_else(|| base.get(name)).cloned(),
        Expr::Bits(bits) => Some(Type::Bits(Some(
            (bits.len() - 2) as u32 * if bits.as_bytes()[1] == b'x' { 4 } else { 1 },
        ))),
        Expr::Bool(_) => Some(Type::Bool),
        Expr::Integer(_, width) => Some(Type::Integer(*width)),
        Expr::Struct(name, _) => Some(Type::Struct(name.clone())),
        Expr::Op(name, width, _) if name == "@slice" || name == "@zero_extend" => {
            Some(Type::Bits(*width))
        }
        _ => None,
    }
}

/// Infer bit widths within basic blocks and replace width-preserving operations
/// with host-integer operations. Unknown widths retain the arbitrary-width path.
pub fn lower_bitvectors(
    function: &mut Function,
    declarations: &BTreeMap<String, (Vec<Type>, Type)>,
    globals: &BTreeMap<String, Type>,
) {
    let targets: BTreeSet<_> = function
        .code
        .iter()
        .filter_map(|op| match op {
            Op::Branch(_, n) | Op::Jump(n) => Some(*n),
            _ => None,
        })
        .collect();
    let mut base = globals.clone();
    base.extend(function.locals.clone());
    base.extend(function.args.iter().cloned());
    let mut types = BTreeMap::new();
    for (i, op) in function.code.iter_mut().enumerate() {
        if targets.contains(&i) {
            types.clear();
        }
        match op {
            Op::Declare(name, ty, init) => {
                types.insert(
                    name.clone(),
                    init.as_ref()
                        .and_then(|e| expression_type(e, &types, &base))
                        .unwrap_or_else(|| ty.clone()),
                );
            }
            Op::Assign(Location::Id(name), expr) => {
                if let Some(ty) = expression_type(expr, &types, &base) {
                    types.insert(name.clone(), ty);
                } else if let Some(ty) = base.get(name) {
                    types.insert(name.clone(), ty.clone());
                }
            }
            Op::Call(Location::Id(name), target, args) => {
                let width = args
                    .first()
                    .and_then(|e| expression_type(e, &types, &base))
                    .and_then(|t| {
                        if let Type::Bits(Some(w)) = t {
                            Some(w)
                        } else {
                            None
                        }
                    });
                let result = match target {
                    Target::Intrinsic(operation)
                        if matches!(
                            operation.as_str(),
                            "add_bits"
                                | "sub_bits"
                                | "add_bits_int"
                                | "sub_bits_int"
                                | "not_bits"
                                | "and_bits"
                                | "or_bits"
                                | "xor_bits"
                                | "shiftl"
                                | "shiftr"
                                | "arith_shiftr"
                                | "shift_bits_left"
                                | "shift_bits_right"
                        ) && width.is_some_and(|w| w <= 128) =>
                    {
                        let w = width.unwrap();
                        let operation = match operation.as_str() {
                            "add_bits" | "add_bits_int" => NativeOperation::Add,
                            "sub_bits" | "sub_bits_int" => NativeOperation::Sub,
                            "not_bits" => NativeOperation::Not,
                            "and_bits" => NativeOperation::And,
                            "or_bits" => NativeOperation::Or,
                            "xor_bits" => NativeOperation::Xor,
                            "shiftl" | "shift_bits_left" => NativeOperation::ShiftLeft,
                            "shiftr" | "shift_bits_right" => NativeOperation::ShiftRight,
                            "arith_shiftr" => NativeOperation::ArithmeticShiftRight,
                            _ => unreachable!("guarded native operation"),
                        };
                        *target = Target::Native {
                            operation,
                            width: w,
                        };
                        Some(Type::Bits(Some(w)))
                    }
                    Target::Function(function) => {
                        declarations.get(function).map(|(_, result)| result.clone())
                    }
                    Target::Native { width, .. } => Some(Type::Bits(Some(*width))),
                    _ => base.get(name).cloned(),
                };
                if let Some(ty) = result {
                    types.insert(name.clone(), ty);
                }
            }
            Op::Jump(_) | Op::Branch(_, _) => types.clear(),
            _ => {}
        }
    }
}

fn literal(expr: &Expr) -> bool {
    matches!(
        expr,
        Expr::Integer(..) | Expr::Bits(_) | Expr::Bool(_) | Expr::String(_) | Expr::Unit
    )
}

fn constant_eq(a: &Expr, b: &Expr) -> Option<bool> {
    match (a, b) {
        (Expr::Bool(a), Expr::Bool(b)) => Some(a == b),
        (Expr::String(a), Expr::String(b)) => Some(a == b),
        (Expr::Unit, Expr::Unit) => Some(true),
        (Expr::Integer(a, _), Expr::Integer(b, _)) => a
            .parse::<i128>()
            .ok()
            .zip(b.parse::<i128>().ok())
            .map(|(a, b)| a == b),
        (Expr::Bits(a), Expr::Bits(b)) => {
            if a == b {
                return Some(true);
            }
            let parse = |s: &str| {
                let radix = if s.as_bytes()[1] == b'x' { 16 } else { 2 };
                let width = (s.len() - 2) * if radix == 16 { 4 } else { 1 };
                u128::from_str_radix(&s[2..], radix)
                    .ok()
                    .map(|value| (width, value))
            };
            parse(a).zip(parse(b)).map(|(a, b)| a == b)
        }
        _ => None,
    }
}

fn fold(expr: &mut Expr, constants: &BTreeMap<String, Expr>) {
    match expr {
        Expr::Id(name) => {
            if let Some(value) = constants.get(name) {
                *expr = value.clone();
            }
        }
        Expr::Op(name, _, args) => {
            for arg in args.iter_mut() {
                fold(arg, constants);
            }
            let result = match (name.as_str(), args.as_slice()) {
                ("@not", [Expr::Bool(b)]) => Some(Expr::Bool(!b)),
                ("@and", [Expr::Bool(a), Expr::Bool(b)]) => Some(Expr::Bool(*a && *b)),
                ("@or", [Expr::Bool(a), Expr::Bool(b)]) => Some(Expr::Bool(*a || *b)),
                ("@eq" | "@neq", [a, b]) => {
                    constant_eq(a, b).map(|equal| Expr::Bool(equal == (name == "@eq")))
                }
                ("@iadd" | "@isub", [Expr::Integer(a, None), Expr::Integer(b, None)]) => a
                    .parse::<i128>()
                    .ok()
                    .zip(b.parse::<i128>().ok())
                    .and_then(|(a, b)| {
                        if name == "@iadd" {
                            a.checked_add(b)
                        } else {
                            a.checked_sub(b)
                        }
                    })
                    .map(|n| Expr::Integer(n.to_string(), None)),
                _ => None,
            };
            if let Some(value) = result {
                *expr = value;
            }
        }
        Expr::Struct(_, fields) => {
            for (_, e) in fields {
                fold(e, constants);
            }
        }
        Expr::Field(e, _) | Expr::As(e, _) | Expr::Is(e, _) => fold(e, constants),
        _ => {}
    }
}

/// Propagate constants only within basic blocks. Calls invalidate all constants,
/// since Sail permits mutation through references and architectural globals.
pub fn specialize(function: &mut Function) {
    let targets: BTreeSet<_> = function
        .code
        .iter()
        .filter_map(|op| match op {
            Op::Branch(_, n) | Op::Jump(n) => Some(*n),
            _ => None,
        })
        .collect();
    let mut constants = BTreeMap::new();
    for (i, op) in function.code.iter_mut().enumerate() {
        if targets.contains(&i) {
            constants.clear();
        }
        match op {
            Op::Declare(name, _, value) => {
                constants.remove(name);
                if let Some(value) = value {
                    fold(value, &constants);
                    if literal(value) {
                        constants.insert(name.clone(), value.clone());
                    }
                }
            }
            Op::Assign(Location::Id(name), value) => {
                fold(value, &constants);
                constants.remove(name);
                // Only local variables are safe to propagate across ordinary assignments.
                if function.locals.contains_key(name) && literal(value) {
                    constants.insert(name.clone(), value.clone());
                }
            }
            Op::Call(location, target, args) => {
                for arg in args.iter_mut() {
                    fold(arg, &constants);
                }
                let folded = match (&*location, &*target, args.as_slice()) {
                    (Location::Id(_), Target::Intrinsic(name), [a, b])
                        if matches!(
                            name.as_str(),
                            "eq_bits" | "eq_int" | "eq_bool" | "eq_string" | "eq_anything"
                        ) =>
                    {
                        constant_eq(a, b).map(Expr::Bool)
                    }
                    _ => None,
                };
                if let Some(value) = folded {
                    if let Location::Id(name) = location {
                        constants.insert(name.clone(), value.clone());
                    }
                    *op = Op::Assign(location.clone(), value);
                } else {
                    constants.clear();
                }
            }
            Op::Branch(expr, _) => {
                fold(expr, &constants);
                constants.clear();
            }
            _ => {
                constants.clear();
            }
        }
    }
    remove_dead_paths(function);
}

pub fn remove_dead_paths(function: &mut Function) {
    let len = function.code.len();
    let mut live = vec![false; len];
    let mut todo = vec![0];
    while let Some(i) = todo.pop() {
        if i >= len || live[i] {
            continue;
        }
        live[i] = true;
        match &function.code[i] {
            Op::Jump(n) | Op::Branch(Expr::Bool(true), n) => todo.push(*n),
            Op::Branch(Expr::Bool(false), _) => todo.push(i + 1),
            Op::Branch(_, n) => {
                todo.push(*n);
                todo.push(i + 1);
            }
            Op::Return | Op::Raise(_) | Op::Arbitrary => {}
            _ => todo.push(i + 1),
        }
    }
    let mut mapping = vec![0; len];
    let mut count = 0;
    for (i, &keep) in live.iter().enumerate() {
        mapping[i] = count;
        count += usize::from(keep);
    }
    let old = std::mem::take(&mut function.code);
    for (i, mut op) in old.into_iter().enumerate() {
        if !live[i] {
            continue;
        }
        match &mut op {
            Op::Jump(n) => *n = mapping[*n],
            Op::Branch(Expr::Bool(true), n) => op = Op::Jump(mapping[*n]),
            Op::Branch(Expr::Bool(false), _) => {
                op = Op::Jump(mapping.get(i + 1).copied().unwrap_or(count))
            }
            Op::Branch(_, n) => *n = mapping[*n],
            _ => {}
        }
        function.code.push(op);
    }
}

/// Select one instruction's path through Sail's execute dispatcher. Payload
/// extraction remains explicit and is evaluated with the decoded constructor.
pub fn specialize_constructor(function: &mut Function, constructor: &str) {
    let Some((argument, _)) = function.args.first() else {
        return;
    };
    for op in &mut function.code {
        if let Op::Branch(Expr::Is(expr, tag), _) = op
            && matches!(expr.as_ref(), Expr::Id(name) if name == argument)
        {
            let value = tag != constructor;
            if let Op::Branch(condition, _) = op {
                *condition = Expr::Bool(value);
            }
        }
    }
    specialize(function);
    prune_locals(function);
}

fn prune_locals(function: &mut Function) {
    let live: BTreeSet<_> = function
        .code
        .iter()
        .filter_map(|op| {
            if let Op::Declare(name, _, _) = op {
                Some(name.as_str())
            } else {
                None
            }
        })
        .collect();
    function
        .locals
        .retain(|name, _| live.contains(name.as_str()));
}

/// Specialize an immutable parameter without changing architectural effects.
/// The caller still supplies the original argument list to the resulting IR.
pub fn specialize_argument(function: &mut Function, argument: &str, value: Expr) -> bool {
    fn root(location: &Location) -> &str {
        match location {
            Location::Id(name) => name,
            Location::Field(base, _) | Location::Deref(base) => root(base),
        }
    }
    fn references(expr: &Expr, argument: &str) -> bool {
        match expr {
            Expr::Ref(name) => name == argument,
            Expr::Field(expr, _) | Expr::As(expr, _) | Expr::Is(expr, _) => {
                references(expr, argument)
            }
            Expr::Op(_, _, args) => args.iter().any(|expr| references(expr, argument)),
            Expr::Struct(_, fields) => fields.iter().any(|(_, expr)| references(expr, argument)),
            _ => false,
        }
    }
    if !literal(&value) {
        return false;
    }
    if function.code.iter().any(|op| match op {
        Op::Assign(location, expr) => root(location) == argument || references(expr, argument),
        Op::Call(location, _, args) => {
            root(location) == argument || args.iter().any(|expr| references(expr, argument))
        }
        Op::Declare(name, _, expr) => {
            name == argument || expr.as_ref().is_some_and(|expr| references(expr, argument))
        }
        Op::Branch(expr, _) => references(expr, argument),
        _ => false,
    }) {
        return false;
    }
    if function.code.iter().any(|op| matches!(op,
        Op::Assign(Location::Id(name), _) | Op::Call(Location::Id(name), _, _) | Op::Declare(name, _, _) if name == argument)) {
        return false;
    }
    let constants = BTreeMap::from([(argument.to_owned(), value)]);
    for op in &mut function.code {
        match op {
            Op::Declare(_, _, Some(expr)) | Op::Assign(_, expr) | Op::Branch(expr, _) => {
                fold(expr, &constants)
            }
            Op::Call(_, _, args) => {
                for expr in args {
                    fold(expr, &constants);
                }
            }
            _ => {}
        }
    }
    specialize(function);
    prune_locals(function);
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn widths_and_fallbacks() {
        assert_eq!(
            storage(&Type::Bits(Some(33))),
            Storage::Integer {
                host_bits: 64,
                value_bits: 33,
                signed: false
            }
        );
        assert_eq!(storage(&Type::Bits(Some(129))), Storage::DynamicBits);
        assert_eq!(storage(&Type::Integer(None)), Storage::BigInteger);
    }

    #[test]
    fn lowers_known_bitvector_widths_to_host_operations() {
        let doc = vmlab_hv_sail_ir::parse("val add = \"add_bits\" : (%bv, %bv) -> %bv\nval f : (%bv64, %bv64) -> %bv64\nfn f(a,b) { x : %bv; x = a; y : %bv; y = add(x,b); return = y; end; }").unwrap();
        let p = lower(&doc.definitions).unwrap();
        assert!(p.functions["f"].code.iter().any(|op| matches!(
            op,
            Op::Call(
                _,
                Target::Native {
                    operation: NativeOperation::Add,
                    width: 64
                },
                _
            )
        )));
    }

    #[test]
    fn different_bitvector_literal_spellings_are_not_folded_unequal() {
        let mut expr = Expr::Op(
            "@eq".into(),
            None,
            vec![Expr::Bits("0xff".into()), Expr::Bits("0b11111111".into())],
        );
        fold(&mut expr, &BTreeMap::new());
        assert_eq!(expr, Expr::Bool(true));
    }

    #[test]
    fn argument_specialization_folds_pure_calls_and_rejects_mutable_parameters() {
        let doc = vmlab_hv_sail_ir::parse("val eq = \"eq_bits\" : (%bv, %bv) -> %bool\nval f : (%bv12) -> %i\nfn f(x) { b : %bool; b = eq(x,0x300); jump b goto 5; return = 7; goto 6; return = 9; end; }").unwrap();
        let program = lower(&doc.definitions).unwrap();
        let mut function = program.functions["f"].clone();
        assert!(specialize_argument(
            &mut function,
            "x",
            Expr::Bits("0x300".into())
        ));
        assert!(
            !function
                .code
                .iter()
                .any(|op| matches!(op, Op::Branch(_, _)))
        );
        assert!(
            !function
                .code
                .iter()
                .any(|op| matches!(op, Op::Call(_, _, _)))
        );
        let doc =
            vmlab_hv_sail_ir::parse("val f : (%i) -> %i\nfn f(x) { x = 3; return = x; end; }")
                .unwrap();
        let mut function = lower(&doc.definitions)
            .unwrap()
            .functions
            .remove("f")
            .unwrap();
        assert!(!specialize_argument(
            &mut function,
            "x",
            Expr::Integer("9".into(), None)
        ));
    }

    #[test]
    fn folding_and_branch_relocation() {
        let doc = vmlab_hv_sail_ir::parse("val f : () -> %i\nfn f() { x : %bool = @not(false); jump x goto 4; return = 7; goto 5; return = 9; end; }").unwrap();
        let p = lower(&doc.definitions).unwrap();
        let f = &p.functions["f"];
        assert_eq!(f.code.len(), 4);
        assert_eq!(f.code[1], Op::Jump(2));
        assert_eq!(
            f.code[2],
            Op::Assign(
                Location::Id("return".into()),
                Expr::Integer("9".into(), None)
            )
        );
    }

    #[test]
    fn does_not_propagate_across_join_or_call() {
        let doc = vmlab_hv_sail_ir::parse("val f : (%bool) -> %bool\nval g : () -> %unit\nfn f(a) { x : %bool = true; jump a goto 3; x = false; u : %unit; u = g(); return = x; end; }").unwrap();
        let p = lower(&doc.definitions).unwrap();
        assert_eq!(
            p.functions["f"].code[5],
            Op::Assign(Location::Id("return".into()), Expr::Id("x".into()))
        );
    }
}
