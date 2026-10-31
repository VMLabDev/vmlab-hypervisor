use crate::{Expr, Function, Location, Op, Target, Type};
use std::collections::BTreeMap;

#[derive(Clone, Debug)]
pub enum Operand {
    Local(usize),
    Global(String),
    Constant(usize),
    Ref(String),
    Struct(String, Vec<(String, Operand)>),
    Field(Box<Operand>, String),
    Is(Box<Operand>, String),
    As(Box<Operand>, String),
    Intrinsic(String, Option<u32>, Vec<Operand>),
}

#[derive(Clone, Debug)]
pub enum Place {
    Local(usize),
    Global(String),
    Field(Box<Place>, String),
    Deref(Box<Place>),
}

#[derive(Clone, Debug)]
pub enum IndexedOp {
    Declare(usize, Type, Option<Operand>),
    Assign(Place, Operand),
    Call(Place, Target, Vec<Operand>),
    Branch(Operand, usize),
    Jump(usize),
    Return,
    Raise(String),
    Specialize,
}

/// Stable local slots and a predecoded literal pool. Branch addresses retain
/// their original operation indices, including no-op specialization markers.
#[derive(Clone, Debug)]
pub struct IndexedFunction {
    pub names: Vec<String>,
    pub slots: BTreeMap<String, usize>,
    pub arguments: Vec<usize>,
    pub result: usize,
    pub constants: Vec<Expr>,
    pub code: Vec<IndexedOp>,
}

pub fn index(function: &Function) -> IndexedFunction {
    let mut names: Vec<_> = function.locals.keys().cloned().collect();
    names.extend(function.args.iter().map(|(name, _)| name.clone()));
    names.push("return".into());
    for op in &function.code {
        if let Op::Declare(name, _, _) = op {
            names.push(name.clone());
        }
    }
    names.sort();
    names.dedup();
    let slots: BTreeMap<_, _> = names
        .iter()
        .cloned()
        .enumerate()
        .map(|(i, name)| (name, i))
        .collect();
    let mut output = IndexedFunction {
        arguments: function.args.iter().map(|(name, _)| slots[name]).collect(),
        result: slots["return"],
        names,
        slots,
        constants: Vec::new(),
        code: Vec::new(),
    };
    for op in &function.code {
        let lowered = match op {
            Op::Declare(name, ty, expr) => IndexedOp::Declare(
                output.slots[name],
                ty.clone(),
                expr.as_ref().map(|e| output.expression(e)),
            ),
            Op::Assign(place, expr) => {
                IndexedOp::Assign(output.place(place), output.expression(expr))
            }
            Op::Call(place, target, args) => IndexedOp::Call(
                output.place(place),
                target.clone(),
                args.iter().map(|e| output.expression(e)).collect(),
            ),
            Op::Branch(expr, target) => IndexedOp::Branch(output.expression(expr), *target),
            Op::Jump(target) => IndexedOp::Jump(*target),
            Op::Return | Op::Arbitrary => IndexedOp::Return,
            Op::Raise(cause) => IndexedOp::Raise(cause.clone()),
            Op::Specialize(_, _) => IndexedOp::Specialize,
        };
        output.code.push(lowered);
    }
    output
}

impl IndexedFunction {
    fn expression(&mut self, expr: &Expr) -> Operand {
        match expr {
            Expr::Id(name) => self.slots.get(name).map_or_else(
                || Operand::Global(name.clone()),
                |slot| Operand::Local(*slot),
            ),
            Expr::Ref(name) => Operand::Ref(name.clone()),
            Expr::Struct(name, fields) => Operand::Struct(
                name.clone(),
                fields
                    .iter()
                    .map(|(field, e)| (field.clone(), self.expression(e)))
                    .collect(),
            ),
            Expr::Field(base, field) => {
                Operand::Field(Box::new(self.expression(base)), field.clone())
            }
            Expr::Is(base, tag) => Operand::Is(Box::new(self.expression(base)), tag.clone()),
            Expr::As(base, tag) => Operand::As(Box::new(self.expression(base)), tag.clone()),
            Expr::Op(name, width, args) => Operand::Intrinsic(
                name.clone(),
                *width,
                args.iter().map(|e| self.expression(e)).collect(),
            ),
            _ => {
                let slot = self.constants.len();
                self.constants.push(expr.clone());
                Operand::Constant(slot)
            }
        }
    }

    fn place(&self, location: &Location) -> Place {
        match location {
            Location::Id(name) => self
                .slots
                .get(name)
                .map_or_else(|| Place::Global(name.clone()), |slot| Place::Local(*slot)),
            Location::Field(base, field) => Place::Field(Box::new(self.place(base)), field.clone()),
            Location::Deref(base) => Place::Deref(Box::new(self.place(base))),
        }
    }
}
