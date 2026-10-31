use super::*;
use vmlab_hv_tcg_ir::NativeOperation;
use vmlab_hv_tcg_ir::native::Unary;
use vmlab_hv_tcg_ir::native::block::{DeferredOperation, Recipe};

impl Recorder {
    fn value_type(&self, id: ValueId) -> Type {
        match &self.block.values[id] {
            Recipe::Scalar { ty, .. } => match ty {
                ScalarType::Bool => Type::Bool,
                ScalarType::Bits(w) => Type::Bits(Some((*w).into())),
                ScalarType::Integer => Type::Integer(None),
            },
            Recipe::Constant(id) => match &self.constants[*id] {
                Value::Bool(_) => Type::Bool,
                Value::Bits { width, .. } => Type::Bits(Some(*width)),
                Value::Integer(_) => Type::Integer(None),
                _ => Type::Unit,
            },
            Recipe::Deferred { ty, .. } => ty.clone(),
            _ => Type::Unit,
        }
    }

    fn bits_width(&self, id: ValueId) -> Option<u32> {
        if let Type::Bits(Some(w)) = self.value_type(id) {
            Some(w)
        } else {
            None
        }
    }

    /// Reduce unbounded arithmetic only at an explicit modulo-2^width use.
    fn lower_bits(&mut self, id: ValueId, width: u16) -> Option<ScalarId> {
        if let Some((id, _)) = self.scalar(id) {
            return self
                .block
                .graph
                .unary(ScalarType::Bits(width), Unary::Truncate(width), id);
        }
        let Recipe::Deferred {
            operation: DeferredOperation::Intrinsic { name, .. },
            args,
            ..
        } = self.block.values[id].clone()
        else {
            return None;
        };
        let operation = match name.as_str() {
            "@iadd" | "add_int" => Binary::Add,
            "@isub" | "sub_int" => Binary::Sub,
            "mult_int" => Binary::Mul,
            _ => return None,
        };
        let a = self.lower_bits(args[0], width)?;
        let b = self.lower_bits(args[1], width)?;
        self.block
            .graph
            .binary(ScalarType::Bits(width), operation, a, b)
    }

    pub(super) fn intrinsic_value<M: Memory>(
        &mut self,
        interpreter: &mut Interpreter<'_, M>,
        name: &str,
        width: Option<u32>,
        args: Vec<ValueId>,
    ) -> Option<ValueId> {
        if !vmlab_hv_tcg_ir::pure_intrinsic(name)
            && !matches!(
                name,
                "instr_announce" | "branch_announce" | "event_callback"
            )
        {
            return None;
        }
        if let Some(value) = self.try_intrinsic(interpreter, name, width, &args) {
            return Some(value);
        }
        // These operations are total for the checked value kinds. Keep wide
        // observation packets and unbounded math lazy until an actual use.
        let ty = match name {
            "@iadd" | "add_int" | "@isub" | "sub_int" | "mult_int"
                if args.len() == 2
                    && args.iter().all(|v| {
                        matches!(self.value_type(*v), Type::Bits(_) | Type::Integer(_))
                    }) =>
            {
                Type::Integer(None)
            }
            "@concat" | "append" | "append_64" if args.len() == 2 => Type::Bits(Some(
                self.bits_width(args[0])?
                    .checked_add(self.bits_width(args[1])?)?,
            )),
            "@unsigned" | "sail_unsigned" | "@signed" | "sail_signed"
                if args.len() == 1 && self.bits_width(args[0]).is_some() =>
            {
                Type::Integer(None)
            }
            _ => return None,
        };
        Some(self.block.value(Recipe::Deferred {
            operation: DeferredOperation::Intrinsic {
                name: name.into(),
                width,
            },
            args,
            ty,
        }))
    }

    fn try_intrinsic<M: Memory>(
        &mut self,
        interpreter: &mut Interpreter<'_, M>,
        name: &str,
        width: Option<u32>,
        args: &[ValueId],
    ) -> Option<ValueId> {
        if let Some(values) = args
            .iter()
            .map(|id| self.known(*id))
            .collect::<Option<Vec<_>>>()
        {
            return Some(self.constant(interpreter.intrinsic(name, width, &values).ok()?));
        }
        let a = *args.first()?;
        let index = |this: &Self, i: usize| this.known(*args.get(i)?).and_then(|v| v.index().ok());
        let scalar = match name {
            "vector_update_subrange" => {
                let hi = index(self, 1)?;
                let lo = index(self, 2)?;
                let (old, ScalarType::Bits(width)) = self.scalar(a)? else {
                    return None;
                };
                if hi < lo || hi >= usize::from(width) {
                    return None;
                }
                let (new, ScalarType::Bits(_)) = self.scalar(args[3])? else {
                    return None;
                };
                let middle_width = (hi - lo + 1) as u16;
                let middle = self.block.graph.slice(new, 0, middle_width);
                let lower = if lo == 0 {
                    middle
                } else {
                    let low = self.block.graph.slice(old, 0, lo as u16);
                    self.block
                        .graph
                        .concat(middle, low, lo as u16, hi as u16 + 1)
                };
                if hi + 1 == usize::from(width) {
                    lower
                } else {
                    let high = self
                        .block
                        .graph
                        .slice(old, hi as u16 + 1, width - hi as u16 - 1);
                    self.block.graph.concat(high, lower, hi as u16 + 1, width)
                }
            }
            "zsail_assert" => {
                let (condition, ScalarType::Bool) = self.scalar(a)? else {
                    return None;
                };
                if self.guards.insert((condition, true)) {
                    let exit = self.snapshot(None, None);
                    self.block
                        .events
                        .push(vmlab_hv_tcg_ir::native::block::Event::Guard {
                            condition,
                            expected: true,
                            exit,
                        });
                }
                return Some(self.constant(Value::Unit));
            }
            "%i->%i64" => {
                let (scalar, ty) = self.scalar(a)?;
                let range = self.block.graph.scalars[scalar].range?;
                if range.min < i128::from(i64::MIN)
                    || range.max > i128::from(i64::MAX)
                    || ty != ScalarType::Integer
                {
                    return None;
                }
                return Some(a);
            }
            "%i64->%i" | "monomorphize" => return Some(a),
            "length" => {
                let length = self.bits_width(a).map(|w| w as usize).or_else(|| {
                    match &self.block.values[a] {
                        Recipe::Vector(v) => Some(v.len()),
                        _ => None,
                    }
                })?;
                return Some(self.constant(Value::int(length)));
            }
            "@unsigned" | "sail_unsigned" => {
                let (a, ScalarType::Bits(w)) = self.scalar(a)? else {
                    return None;
                };
                if w == 128 {
                    return None;
                }
                // Preserve the proven unsigned range while changing the Sail value kind.
                let zero = self.block.graph.constant(ScalarType::Integer, 0);
                self.block
                    .graph
                    .binary(ScalarType::Integer, Binary::Add, a, zero)?
            }
            "@signed" | "sail_signed" => {
                let (a, ScalarType::Bits(w)) = self.scalar(a)? else {
                    return None;
                };
                self.block
                    .graph
                    .unary(ScalarType::Integer, Unary::SignExtend(w), a)?
            }
            "@not" | "not" => {
                let (a, _) = self.scalar(a)?;
                self.block
                    .graph
                    .unary(ScalarType::Bool, Unary::BoolNot, a)?
            }
            "@bvnot" | "not_bits" => {
                let (a, ty) = self.scalar(a)?;
                self.block.graph.unary(ty, Unary::BitNot, a)?
            }
            "neg_int" => {
                let (a, _) = self.scalar(a)?;
                self.block
                    .graph
                    .unary(ScalarType::Integer, Unary::Negate, a)?
            }
            "tmod_int" | "tdiv_int" => {
                let (a, _) = self.scalar(a)?;
                let (b, _) = self.scalar(args[1])?;
                let unsigned = self.block.graph.scalars[a].range?.min >= 0
                    && self.block.graph.scalars[b].range?.min > 0;
                let operation = match (name, unsigned) {
                    ("tmod_int", true) => Binary::RemainderUnsigned,
                    ("tmod_int", false) => Binary::RemainderSigned,
                    (_, true) => Binary::DivideUnsigned,
                    (_, false) => Binary::DivideSigned,
                };
                self.block
                    .graph
                    .binary(ScalarType::Integer, operation, a, b)?
            }
            "@eq" | "eq_int" | "eq_bool" | "eq_bits" | "eq_string" | "eq_anything" | "@neq"
            | "neq_bits" => {
                let (a, at) = self.scalar(a)?;
                let (b, bt) = self.scalar(args[1])?;
                let neq = matches!(name, "@neq" | "neq_bits");
                if at != bt {
                    return Some(self.constant(Value::Bool(neq)));
                }
                self.block.graph.binary(
                    ScalarType::Bool,
                    if neq { Binary::NotEqual } else { Binary::Equal },
                    a,
                    b,
                )?
            }
            "@lt" | "lt" | "@lteq" | "lteq" | "@gt" | "gt" | "@gteq" | "gteq" => {
                let (mut a, at) = self.scalar(a)?;
                let (mut b, bt) = self.scalar(args[1])?;
                if at == ScalarType::Bits(128) || bt == ScalarType::Bits(128) {
                    return None;
                }
                if matches!(name, "@gt" | "gt" | "@gteq" | "gteq") {
                    std::mem::swap(&mut a, &mut b);
                }
                let op = if matches!(name, "@lt" | "lt" | "@gt" | "gt") {
                    Binary::LessSigned
                } else {
                    Binary::LessEqualSigned
                };
                self.block.graph.binary(ScalarType::Bool, op, a, b)?
            }
            "@and" | "and_bool" | "@or" | "or_bool" | "@iadd" | "add_int" | "@isub" | "sub_int"
            | "mult_int" | "@bvor" | "or_bits" | "@bvand" | "and_bits" | "@bvxor" | "xor_bits"
            | "@bvadd" | "add_bits" | "add_bits_int" | "@bvsub" | "sub_bits" | "sub_bits_int" => {
                let (a, mut ty) = self.scalar(a)?;
                let (b, bt) = self.scalar(args[1])?;
                if matches!(name, "@iadd" | "add_int" | "@isub" | "sub_int" | "mult_int") {
                    if ty == ScalarType::Bits(128) || bt == ScalarType::Bits(128) {
                        return None;
                    }
                    ty = ScalarType::Integer;
                }
                let op = match name {
                    "@and" | "and_bool" | "@bvand" | "and_bits" => Binary::And,
                    "@or" | "or_bool" | "@bvor" | "or_bits" => Binary::Or,
                    "@bvxor" | "xor_bits" => Binary::Xor,
                    "@isub" | "sub_int" | "@bvsub" | "sub_bits" | "sub_bits_int" => Binary::Sub,
                    "mult_int" => Binary::Mul,
                    _ => Binary::Add,
                };
                self.block.graph.binary(ty, op, a, b)?
            }
            "@slice" | "vector_subrange" | "get_slice_int" => {
                let (value, offset, w) = match name {
                    "@slice" => (a, index(self, 1)?, width? as usize),
                    "vector_subrange" => {
                        let hi = index(self, 1)?;
                        let lo = index(self, 2)?;
                        (a, lo, hi.checked_sub(lo)? + 1)
                    }
                    _ => (args[1], index(self, 2)?, index(self, 0)?),
                };
                if w > 128 {
                    return None;
                }
                if name == "get_slice_int" {
                    if offset.checked_add(w)? > 128 {
                        return None;
                    }
                    let source = self.lower_bits(value, 128)?;
                    self.block
                        .graph
                        .slice(source, u16::try_from(offset).ok()?, w as u16)
                } else {
                    let (source, _) = self.scalar(value)?;
                    self.block
                        .graph
                        .slice(source, u16::try_from(offset).ok()?, w as u16)
                }
            }
            "sign_extend" | "zero_extend" | "sail_truncate" | "@zero_extend" => {
                let w = if name == "@zero_extend" {
                    width? as usize
                } else {
                    index(self, 1)?
                };
                if w > 128 {
                    return None;
                }
                let (a, ScalarType::Bits(source)) = self.scalar(a)? else {
                    return None;
                };
                self.block.graph.unary(
                    ScalarType::Bits(w as u16),
                    if name == "sign_extend" {
                        Unary::SignExtend(source)
                    } else {
                        Unary::Truncate(w as u16)
                    },
                    a,
                )?
            }
            "@concat" | "append" | "append_64" => {
                let (a, ScalarType::Bits(wa)) = self.scalar(a)? else {
                    return None;
                };
                let (b, ScalarType::Bits(wb)) = self.scalar(args[1])? else {
                    return None;
                };
                if u32::from(wa) + u32::from(wb) > 128 {
                    return None;
                }
                self.block.graph.concat(a, b, wb, wa + wb)
            }
            "shiftl" | "shift_bits_left" | "shiftr" | "shift_bits_right" | "arith_shiftr" => {
                let (a, ty) = self.scalar(a)?;
                let (b, _) = self.scalar(args[1])?;
                if self.block.graph.scalars[b].range?.min < 0 {
                    return None;
                }
                let op = match name {
                    "shiftl" | "shift_bits_left" => Binary::ShiftLeft,
                    "arith_shiftr" => Binary::ArithmeticShiftRight,
                    _ => Binary::ShiftRight,
                };
                self.block.graph.binary(ty, op, a, b)?
            }
            "count_leading_zeros" | "count_trailing_zeros" => {
                let (a, ScalarType::Bits(w)) = self.scalar(a)? else {
                    return None;
                };
                self.block.graph.unary(
                    ScalarType::Integer,
                    if name == "count_leading_zeros" {
                        Unary::LeadingZeros(w)
                    } else {
                        Unary::TrailingZeros(w)
                    },
                    a,
                )?
            }
            "vector_access" | "@bvaccess" => {
                let i = index(self, 1)?;
                if let Recipe::Vector(values) = &self.block.values[a] {
                    return values.get(i).copied();
                }
                let (a, ScalarType::Bits(w)) = self.scalar(a)? else {
                    return None;
                };
                if i >= usize::from(w) {
                    return None;
                }
                self.block.graph.slice(a, i as u16, 1)
            }
            "vector_init" | "undefined_vector" => {
                return Some(
                    self.block
                        .value(Recipe::Vector(vec![args[1]; index(self, 0)?])),
                );
            }
            "zinternal_vector_init" => {
                let value = self.constant(Value::Undefined);
                return Some(
                    self.block
                        .value(Recipe::Vector(vec![value; index(self, 0)?])),
                );
            }
            "vector_update" | "zinternal_vector_update" | "zupdate_fbits" => {
                let index = index(self, 1)?;
                let vector = match self.block.values[a].clone() {
                    Recipe::Vector(values) => Some(values),
                    _ => match self.known(a) {
                        Some(Value::Vector(values)) => {
                            Some(values.into_iter().map(|v| self.constant(v)).collect())
                        }
                        _ => None,
                    },
                };
                if let Some(mut values) = vector {
                    *values.get_mut(index)? = args[2];
                    return Some(self.block.value(Recipe::Vector(values)));
                }
                let (old, ScalarType::Bits(width)) = self.scalar(a)? else {
                    return None;
                };
                if index >= usize::from(width) {
                    return if name == "zupdate_fbits" {
                        Some(a)
                    } else {
                        None
                    };
                }
                let ty = ScalarType::Bits(width);
                let (new, ScalarType::Bits(_)) = self.scalar(args[2])? else {
                    return None;
                };
                let zero = self
                    .block
                    .graph
                    .constant(self.block.graph.scalars[new].ty, 0);
                let bit = self
                    .block
                    .graph
                    .binary(ScalarType::Bool, Binary::NotEqual, new, zero)?;
                let shift = self
                    .block
                    .graph
                    .constant(ScalarType::Integer, index as u128);
                let bit = self.block.graph.binary(ty, Binary::ShiftLeft, bit, shift)?;
                let mask = self
                    .block
                    .graph
                    .constant(ty, vmlab_hv_tcg_ir::native::mask(width) ^ (1u128 << index));
                let old = self.block.graph.binary(ty, Binary::And, old, mask)?;
                self.block.graph.binary(ty, Binary::Or, old, bit)?
            }
            "instr_announce" | "branch_announce" | "event_callback" => {
                return Some(self.constant(Value::Unit));
            }
            _ => return None,
        };
        let ty = match name {
            "@unsigned"
            | "sail_unsigned"
            | "@signed"
            | "sail_signed"
            | "count_leading_zeros"
            | "count_trailing_zeros"
            | "@iadd"
            | "add_int"
            | "@isub"
            | "sub_int"
            | "mult_int"
            | "neg_int"
            | "tmod_int"
            | "tdiv_int" => ScalarType::Integer,
            _ => self.block.graph.scalars[scalar].ty,
        };
        Some(self.scalar_value(scalar, ty))
    }

    pub(super) fn native_value(
        &mut self,
        operation: &NativeOperation,
        width: u32,
        args: &[ValueId],
    ) -> Option<ValueId> {
        let width = u16::try_from(width).ok().filter(|w| *w <= 128)?;
        let a = self.lower_bits(args[0], width)?;
        let ty = ScalarType::Bits(width);
        let value = if *operation == NativeOperation::Not {
            self.block.graph.unary(ty, Unary::BitNot, a)?
        } else {
            let (b, _) = self.scalar(args[1])?;
            let op = match operation {
                NativeOperation::Add => Binary::Add,
                NativeOperation::Sub => Binary::Sub,
                NativeOperation::And => Binary::And,
                NativeOperation::Or => Binary::Or,
                NativeOperation::Xor => Binary::Xor,
                NativeOperation::ShiftLeft => Binary::ShiftLeft,
                NativeOperation::ShiftRight => Binary::ShiftRight,
                NativeOperation::ArithmeticShiftRight => Binary::ArithmeticShiftRight,
                NativeOperation::Not => unreachable!(),
            };
            self.block.graph.binary(ty, op, a, b)?
        };
        Some(self.scalar_value(value, ty))
    }
}
