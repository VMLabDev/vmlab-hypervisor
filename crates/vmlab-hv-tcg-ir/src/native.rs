//! Guarded scalar SSA shared by whole-block specialization and native backends.
mod assume;
mod bitfield;
pub mod block;
pub mod dce;
pub mod linear;
pub mod liveness;
mod modular;
pub mod packed;
pub mod schedule;
use std::collections::HashMap;

pub type ScalarId = usize;
pub type InputId = usize;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ScalarType {
    Bool,
    Bits(u16),
    Integer,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Range {
    pub min: i128,
    pub max: i128,
}

impl Range {
    pub fn singleton(value: i128) -> Self {
        Self {
            min: value,
            max: value,
        }
    }

    pub fn contains(self, value: i128) -> bool {
        self.min <= value && value <= self.max
    }

    fn add(self, other: Self) -> Option<Self> {
        Some(Self {
            min: self.min.checked_add(other.min)?,
            max: self.max.checked_add(other.max)?,
        })
    }

    fn sub(self, other: Self) -> Option<Self> {
        Some(Self {
            min: self.min.checked_sub(other.max)?,
            max: self.max.checked_sub(other.min)?,
        })
    }

    fn mul(self, other: Self) -> Option<Self> {
        let values = [
            self.min.checked_mul(other.min)?,
            self.min.checked_mul(other.max)?,
            self.max.checked_mul(other.min)?,
            self.max.checked_mul(other.max)?,
        ];
        Some(Self {
            min: *values.iter().min()?,
            max: *values.iter().max()?,
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Unary {
    BoolNot,
    BitNot,
    Negate,
    Truncate(u16),
    SignExtend(u16),
    LeadingZeros(u16),
    TrailingZeros(u16),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Binary {
    Add,
    Sub,
    Mul,
    And,
    Or,
    Xor,
    ShiftLeft,
    ShiftRight,
    ArithmeticShiftRight,
    Equal,
    NotEqual,
    LessSigned,
    LessUnsigned,
    LessEqualSigned,
    LessEqualUnsigned,
    DivideSigned,
    RemainderSigned,
    DivideUnsigned,
    RemainderUnsigned,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum Operation {
    Constant(u128),
    Input(InputId),
    /// Same bits as its input, with a range proven by a preceding guard.
    Refine(ScalarId),
    /// Produced once by an ordered memory event, never recomputed on an exit.
    Effect(usize),
    Unary(Unary, ScalarId),
    Binary(Binary, ScalarId, ScalarId),
    Slice {
        value: ScalarId,
        offset: u16,
        width: u16,
    },
    Concat {
        high: ScalarId,
        low: ScalarId,
        low_width: u16,
    },
}

#[derive(Clone, Debug)]
pub struct Scalar {
    pub ty: ScalarType,
    pub range: Option<Range>,
    pub operation: Operation,
}

#[derive(Clone, Debug, Default)]
pub struct Graph {
    pub scalars: Vec<Scalar>,
    interned: HashMap<(ScalarType, Option<Range>, Operation), ScalarId>,
    aliases: HashMap<ScalarId, ScalarId>,
}

pub fn mask(width: u16) -> u128 {
    match width {
        0 => 0,
        128 => u128::MAX,
        n => (1u128 << n) - 1,
    }
}

fn type_range(ty: ScalarType) -> Option<Range> {
    match ty {
        ScalarType::Bool => Some(Range { min: 0, max: 1 }),
        ScalarType::Bits(width) if width < 128 => Some(Range {
            min: 0,
            max: mask(width) as i128,
        }),
        _ => None,
    }
}

impl Graph {
    pub fn retain(&mut self, keep: &[bool]) -> Vec<ScalarId> {
        self.aliases.clear();
        let mut mapping = vec![usize::MAX; self.scalars.len()];
        let mut next = 0;
        for (id, retain) in keep.iter().enumerate() {
            if *retain {
                mapping[id] = next;
                next += 1;
            }
        }
        self.scalars = std::mem::take(&mut self.scalars)
            .into_iter()
            .enumerate()
            .filter_map(|(id, mut scalar)| {
                if !keep[id] {
                    return None;
                }
                match &mut scalar.operation {
                    Operation::Refine(value)
                    | Operation::Unary(_, value)
                    | Operation::Slice { value, .. } => *value = mapping[*value],
                    Operation::Binary(_, a, b)
                    | Operation::Concat {
                        high: a, low: b, ..
                    } => {
                        *a = mapping[*a];
                        *b = mapping[*b];
                    }
                    _ => {}
                }
                Some(scalar)
            })
            .collect();
        self.interned.clear();
        for (id, scalar) in self.scalars.iter().enumerate() {
            self.interned
                .insert((scalar.ty, scalar.range, scalar.operation.clone()), id);
        }
        mapping
    }

    fn insert(&mut self, ty: ScalarType, range: Option<Range>, operation: Operation) -> ScalarId {
        let key = (ty, range, operation.clone());
        if let Some(id) = self.interned.get(&key) {
            return self.resolve(*id);
        }
        let id = self.scalars.len();
        self.scalars.push(Scalar {
            ty,
            range,
            operation,
        });
        self.interned.insert(key, id);
        id
    }

    pub fn constant(&mut self, ty: ScalarType, mut value: u128) -> ScalarId {
        if let ScalarType::Bits(width) = ty {
            value &= mask(width);
        }
        let range = if matches!(ty, ScalarType::Bits(_)) && value > i128::MAX as u128 {
            None
        } else {
            Some(Range::singleton(value as i128))
        };
        self.insert(ty, range, Operation::Constant(value))
    }

    pub fn input(&mut self, input: InputId, ty: ScalarType, range: Option<Range>) -> ScalarId {
        self.insert(
            ty,
            range.or_else(|| type_range(ty)),
            Operation::Input(input),
        )
    }

    pub fn effect(&mut self, effect: usize, ty: ScalarType) -> ScalarId {
        self.insert(ty, type_range(ty), Operation::Effect(effect))
    }

    pub fn literal(&self, id: ScalarId) -> Option<u128> {
        if let Operation::Constant(value) = self.scalars[id].operation {
            Some(value)
        } else {
            None
        }
    }

    fn identity(&mut self, ty: ScalarType, value: ScalarId) -> Option<ScalarId> {
        if self.scalars[value].ty == ty {
            return Some(value);
        }
        if let ScalarType::Bits(width) = ty {
            if !self.scalars[value]
                .range
                .is_some_and(|range| range.min >= 0 && range.max as u128 <= mask(width))
            {
                return self.unary(ty, Unary::Truncate(width), value);
            }
        } else if ty == ScalarType::Integer {
            let range = self.scalars[value].range?;
            if range.min < 0 {
                return None;
            }
        } else if ty == ScalarType::Bool {
            let range = self.scalars[value].range?;
            if range.min < 0 || range.max > 1 {
                return None;
            }
        }
        Some(self.insert(ty, self.scalars[value].range, Operation::Refine(value)))
    }

    pub fn unary(&mut self, ty: ScalarType, operation: Unary, value: ScalarId) -> Option<ScalarId> {
        let value = self.resolve(value);
        if let Some(value) = self.literal(value) {
            return Some(self.constant(ty, evaluate_unary(operation, value)?));
        }
        if let (ScalarType::Bits(target), Unary::Truncate(width)) = (ty, operation)
            && target == width
            && let Some(value) = self.modular(value, width)
        {
            return Some(value);
        }
        if let Some(range) = self.scalars[value].range
            && range.min >= 0
        {
            if let Unary::Truncate(width) = operation
                && range.max as u128 <= mask(width)
            {
                return self.identity(ty, value);
            }
            if let Unary::SignExtend(width) = operation
                && width > 0
                && width <= 128
                && (range.max as u128) < (1u128 << (width - 1))
            {
                return self.identity(ty, value);
            }
        }
        if let Unary::Truncate(width) = operation
            && self.scalars[value].ty == ScalarType::Bits(width)
        {
            return self.identity(ty, value);
        }
        let range = match operation {
            Unary::Negate => {
                let r = self.scalars[value].range?;
                Some(Range {
                    min: r.max.checked_neg()?,
                    max: r.min.checked_neg()?,
                })
            }
            Unary::SignExtend(width) if ty == ScalarType::Integer && width > 0 && width < 128 => {
                Some(Range {
                    min: -(1i128 << (width - 1)),
                    max: (1i128 << (width - 1)) - 1,
                })
            }
            _ => type_range(ty),
        };
        Some(self.insert(ty, range, Operation::Unary(operation, value)))
    }

    pub fn binary(
        &mut self,
        ty: ScalarType,
        operation: Binary,
        left: ScalarId,
        right: ScalarId,
    ) -> Option<ScalarId> {
        let left = self.resolve(left);
        let right = self.resolve(right);
        if self.literal(left).is_some()
            && self.literal(right).is_none()
            && matches!(
                operation,
                Binary::Add
                    | Binary::Mul
                    | Binary::And
                    | Binary::Or
                    | Binary::Xor
                    | Binary::Equal
                    | Binary::NotEqual
            )
        {
            return self.binary(ty, operation, right, left);
        }
        if let (Some(a), Some(b)) = (self.literal(left), self.literal(right)) {
            return Some(self.constant(ty, evaluate_binary(operation, ty, a, b)?));
        }
        if let ScalarType::Bits(width) = ty
            && matches!(operation, Binary::ShiftLeft | Binary::ShiftRight)
            && let Some(count) = self.literal(right)
        {
            let fits = matches!(self.scalars[left].ty, ScalarType::Bits(actual) if actual <= width)
                || self.scalars[left]
                    .range
                    .is_some_and(|range| range.min >= 0 && range.max as u128 <= mask(width));
            if count >= 128
                || (count >= u128::from(width) && (operation == Binary::ShiftLeft || fits))
            {
                return Some(self.constant(ty, 0));
            }
            let mut source = left;
            while let Operation::Refine(previous) = self.scalars[source].operation {
                source = previous;
            }
            if self.scalars[source].ty == ty
                && let Operation::Binary(inner, base, amount) = self.scalars[source].operation
                && let Some(first) = self.literal(amount)
                && first < u128::from(width)
                && count < u128::from(width)
            {
                if inner == Binary::ShiftLeft && operation == Binary::ShiftRight {
                    if count >= first {
                        let slice = self.slice(base, (count - first) as u16, width - count as u16);
                        return self.identity(ty, slice);
                    }
                    let slice = self.slice(base, 0, width - first as u16);
                    let amount = self.constant(ScalarType::Integer, first - count);
                    return self.binary(ty, Binary::ShiftLeft, slice, amount);
                }
                if inner == operation && operation == Binary::ShiftLeft {
                    let amount = self.constant(ScalarType::Integer, first + count);
                    return self.binary(ty, operation, base, amount);
                }
            }
        }
        if matches!(operation, Binary::Equal | Binary::NotEqual)
            && let Some(value) = self.fold_masked_comparison(operation, left, right)
        {
            return Some(value);
        }
        if matches!(operation, Binary::Equal | Binary::NotEqual)
            && let ScalarType::Bits(width) = self.scalars[left].ty
            && let Some(target) = self.literal(right).filter(|value| *value <= mask(width))
            && let Operation::Binary(arithmetic, base, offset) = self.scalars[left].operation
            && let Some(offset) = self.literal(offset)
            && matches!(arithmetic, Binary::Add | Binary::Sub)
        {
            let target = if arithmetic == Binary::Sub {
                target.wrapping_add(offset)
            } else {
                target.wrapping_sub(offset)
            };
            let target = self.constant(ScalarType::Bits(width), target);
            return self.binary(ty, operation, base, target);
        }
        if let (Some(a), Some(b)) = (self.scalars[left].range, self.scalars[right].range) {
            let comparison = match operation {
                Binary::Equal if a.max < b.min || b.max < a.min => Some(false),
                Binary::NotEqual if a.max < b.min || b.max < a.min => Some(true),
                Binary::LessSigned | Binary::LessUnsigned
                    if operation == Binary::LessSigned || (a.min >= 0 && b.min >= 0) =>
                {
                    if a.max < b.min {
                        Some(true)
                    } else if a.min >= b.max {
                        Some(false)
                    } else {
                        None
                    }
                }
                Binary::LessEqualSigned | Binary::LessEqualUnsigned
                    if operation == Binary::LessEqualSigned || (a.min >= 0 && b.min >= 0) =>
                {
                    if a.max <= b.min {
                        Some(true)
                    } else if a.min > b.max {
                        Some(false)
                    } else {
                        None
                    }
                }
                _ => None,
            };
            if let Some(value) = comparison {
                return Some(self.constant(ScalarType::Bool, u128::from(value)));
            }
        }
        if left == right {
            match operation {
                Binary::Equal | Binary::LessEqualSigned | Binary::LessEqualUnsigned => {
                    return Some(self.constant(ScalarType::Bool, 1));
                }
                Binary::NotEqual | Binary::LessSigned | Binary::LessUnsigned => {
                    return Some(self.constant(ScalarType::Bool, 0));
                }
                Binary::Sub | Binary::Xor => return Some(self.constant(ty, 0)),
                Binary::And | Binary::Or => return self.identity(ty, left),
                _ => {}
            }
        }
        if self.literal(right) == Some(0)
            && matches!(
                operation,
                Binary::Add
                    | Binary::Sub
                    | Binary::Or
                    | Binary::Xor
                    | Binary::ShiftLeft
                    | Binary::ShiftRight
                    | Binary::ArithmeticShiftRight
            )
        {
            return self.identity(ty, left);
        }
        if self.literal(right) == Some(0) && matches!(operation, Binary::And | Binary::Mul) {
            return Some(self.constant(ty, 0));
        }
        if let ScalarType::Bits(width) = ty
            && self.literal(right) == Some(mask(width))
            && operation == Binary::And
        {
            return self.identity(ty, left);
        }
        if let ScalarType::Bits(width) = ty
            && operation == Binary::Or
            && self.literal(right) == Some(mask(width))
        {
            return Some(self.constant(ty, mask(width)));
        }
        if ty == ScalarType::Bool && self.literal(right) == Some(1) {
            if operation == Binary::And {
                return Some(left);
            }
            if operation == Binary::Or {
                return Some(right);
            }
        }
        if matches!(ty, ScalarType::Bits(_))
            && let Some(b) = self.literal(right)
            && let Operation::Binary(inner, base, constant) = self.scalars[left].operation
            && let Some(a) = self.literal(constant)
        {
            if inner == operation && matches!(operation, Binary::And | Binary::Or | Binary::Xor) {
                let constant = self.constant(ty, evaluate_binary(operation, ty, a, b)?);
                return self.binary(ty, operation, base, constant);
            }
            if inner == Binary::Or && operation == Binary::And {
                let masked = self.binary(ty, Binary::And, base, right)?;
                let constant = self.constant(ty, a & b);
                return self.binary(ty, Binary::Or, masked, constant);
            }
        }
        let range = if ty == ScalarType::Integer {
            let a = self.scalars[left].range?;
            let b = self.scalars[right].range?;
            Some(match operation {
                Binary::Add => a.add(b)?,
                Binary::Sub => a.sub(b)?,
                Binary::Mul => a.mul(b)?,
                Binary::And if a.min >= 0 && b.min >= 0 => Range {
                    min: 0,
                    max: a.max.min(b.max),
                },
                Binary::DivideUnsigned if a.min >= 0 && b.min > 0 => Range {
                    min: 0,
                    max: a.max / b.min,
                },
                Binary::RemainderUnsigned if a.min >= 0 && b.min > 0 => Range {
                    min: 0,
                    max: a.max.min(b.max - 1),
                },
                Binary::DivideSigned | Binary::RemainderSigned
                    if (b.min > 0 || b.max < 0) && !(a.contains(i128::MIN) && b.contains(-1)) =>
                {
                    if operation == Binary::RemainderSigned {
                        let bound = b.min.checked_abs()?.max(b.max.checked_abs()?) - 1;
                        Range {
                            min: a.min.max(-bound).min(0),
                            max: a.max.min(bound).max(0),
                        }
                    } else {
                        let values = [
                            a.min.checked_div(b.min)?,
                            a.min.checked_div(b.max)?,
                            a.max.checked_div(b.min)?,
                            a.max.checked_div(b.max)?,
                        ];
                        Range {
                            min: *values.iter().min()?,
                            max: *values.iter().max()?,
                        }
                    }
                }
                _ => return None,
            })
        } else if let ScalarType::Bits(width) = ty {
            let refined = self.scalars[left]
                .range
                .zip(self.scalars[right].range)
                .and_then(|(a, b)| match operation {
                    Binary::Add => a.add(b),
                    Binary::Sub => a.sub(b),
                    Binary::Mul => a.mul(b),
                    Binary::And if a.min >= 0 && b.min >= 0 => Some(Range {
                        min: 0,
                        max: a.max.min(b.max),
                    }),
                    Binary::ShiftLeft if a.min >= 0 && b.min >= 0 && b.max < 127 => a
                        .min
                        .checked_mul(1i128 << b.min as u32)
                        .zip(a.max.checked_mul(1i128 << b.max as u32))
                        .map(|(min, max)| Range { min, max }),
                    Binary::ShiftRight if a.min >= 0 && b.min >= 0 => Some(Range {
                        min: if b.max >= 128 {
                            0
                        } else {
                            a.min >> b.max as u32
                        },
                        max: if b.min >= 128 {
                            0
                        } else {
                            a.max >> b.min as u32
                        },
                    }),
                    _ => None,
                })
                .filter(|range| width < 128 && range.min >= 0 && range.max <= mask(width) as i128);
            refined.or_else(|| type_range(ty))
        } else {
            type_range(ty)
        };
        if operation == Binary::RemainderUnsigned
            && let Some(divisor) = self.literal(right)
            && divisor.is_power_of_two()
        {
            let bits = self.constant(ty, divisor - 1);
            return self.binary(ty, Binary::And, left, bits);
        }
        if operation == Binary::Add
            && let Some(b) = self.literal(right)
            && let Operation::Binary(Binary::Add, base, increment) = self.scalars[left].operation
            && let Some(a) = self.literal(increment)
        {
            let increment = self.constant(ty, evaluate_binary(Binary::Add, ty, a, b)?);
            return self.binary(ty, Binary::Add, base, increment);
        }
        if operation == Binary::Sub
            && let Some(b) = self.literal(right)
            && let Operation::Binary(Binary::Sub, base, decrement) = self.scalars[left].operation
            && let Some(a) = self.literal(decrement)
            && let Some(sum) = evaluate_binary(Binary::Add, ty, a, b)
        {
            let decrement = self.constant(ty, sum);
            return self.binary(ty, Binary::Sub, base, decrement);
        }
        Some(self.insert(ty, range, Operation::Binary(operation, left, right)))
    }

    pub fn slice(&mut self, value: ScalarId, offset: u16, width: u16) -> ScalarId {
        let value = self.resolve(value);
        if let Some(value) = self.literal(value) {
            return self.constant(
                ScalarType::Bits(width),
                value.checked_shr(offset.into()).unwrap_or(0),
            );
        }
        if offset == 0
            && let Some(value) = self.modular(value, width)
        {
            return value;
        }
        if let Some(range) = self.scalars[value].range
            && range.min >= 0
        {
            let upper = if offset >= 128 {
                0
            } else {
                (range.max as u128) >> offset
            };
            if upper == 0 {
                return self.constant(ScalarType::Bits(width), 0);
            }
            if offset == 0 && upper <= mask(width) {
                return self
                    .identity(ScalarType::Bits(width), value)
                    .expect("bounded bitvector view");
            }
        }
        if offset >= 128 || matches!(self.scalars[value].ty, ScalarType::Bits(w) if offset >= w) {
            return self.constant(ScalarType::Bits(width), 0);
        }
        if offset == 0 && self.scalars[value].ty == ScalarType::Bits(width) {
            return value;
        }
        match self.scalars[value].operation.clone() {
            Operation::Slice {
                value,
                offset: base,
                width: source_width,
            } if offset + width <= source_width => return self.slice(value, base + offset, width),
            Operation::Concat {
                high,
                low,
                low_width,
            } => {
                if offset >= low_width {
                    return self.slice(high, offset - low_width, width);
                }
                if offset + width <= low_width {
                    return self.slice(low, offset, width);
                }
                let lower_width = low_width - offset;
                let lower = self.slice(low, offset, lower_width);
                let upper = self.slice(high, 0, width - lower_width);
                return self.concat(upper, lower, lower_width, width);
            }
            _ => {}
        }
        self.insert(
            ScalarType::Bits(width),
            type_range(ScalarType::Bits(width)),
            Operation::Slice {
                value,
                offset,
                width,
            },
        )
    }

    pub fn concat(
        &mut self,
        high: ScalarId,
        low: ScalarId,
        low_width: u16,
        width: u16,
    ) -> ScalarId {
        let high = self.resolve(high);
        let low = self.resolve(low);
        if width == low_width {
            return self.slice(low, 0, width);
        }
        if low_width == 0 {
            return self.slice(high, 0, width);
        }
        if let (Some(high), Some(low)) = (self.literal(high), self.literal(low)) {
            return self.constant(
                ScalarType::Bits(width),
                high.checked_shl(low_width.into()).unwrap_or(0) | low,
            );
        }
        if let (
            Operation::Slice {
                value: a,
                offset: hi,
                ..
            },
            Operation::Slice {
                value: b,
                offset: lo,
                ..
            },
        ) = (&self.scalars[high].operation, &self.scalars[low].operation)
            && a == b
            && *hi == *lo + low_width
        {
            return self.slice(*a, *lo, width);
        }
        if let Some(value) = self.fold_bitfield_concat(high, low, low_width, width) {
            return value;
        }
        self.insert(
            ScalarType::Bits(width),
            type_range(ScalarType::Bits(width)),
            Operation::Concat {
                high,
                low,
                low_width,
            },
        )
    }
}

pub fn evaluate_unary(operation: Unary, value: u128) -> Option<u128> {
    Some(match operation {
        Unary::BoolNot => u128::from(value == 0),
        Unary::BitNot => !value,
        Unary::Negate => (value as i128).checked_neg()? as u128,
        Unary::Truncate(width) => value & mask(width),
        Unary::SignExtend(0) => 0,
        Unary::SignExtend(width) => (((value << (128 - width)) as i128) >> (128 - width)) as u128,
        Unary::LeadingZeros(width) => {
            u128::from((value & mask(width)).leading_zeros() - (128 - u32::from(width)))
        }
        Unary::TrailingZeros(width) => {
            u128::from((value & mask(width)).trailing_zeros().min(width.into()))
        }
    })
}

pub fn evaluate_binary(operation: Binary, ty: ScalarType, a: u128, b: u128) -> Option<u128> {
    let value = match operation {
        Binary::Add if ty == ScalarType::Integer => (a as i128).checked_add(b as i128)? as u128,
        Binary::Sub if ty == ScalarType::Integer => (a as i128).checked_sub(b as i128)? as u128,
        Binary::Mul if ty == ScalarType::Integer => (a as i128).checked_mul(b as i128)? as u128,
        Binary::Add => a.wrapping_add(b),
        Binary::Sub => a.wrapping_sub(b),
        Binary::Mul => a.wrapping_mul(b),
        Binary::And => a & b,
        Binary::Or => a | b,
        Binary::Xor => a ^ b,
        Binary::ShiftLeft => a
            .checked_shl(u32::try_from(b).unwrap_or(u32::MAX))
            .unwrap_or(0),
        Binary::ShiftRight => a
            .checked_shr(u32::try_from(b).unwrap_or(u32::MAX))
            .unwrap_or(0),
        Binary::ArithmeticShiftRight => {
            let width = if let ScalarType::Bits(width) = ty {
                width
            } else {
                128
            };
            if width == 0 {
                0
            } else {
                let signed = ((a << (128 - width)) as i128) >> (128 - width);
                (signed >> b.min(u128::from(width - 1))) as u128
            }
        }
        Binary::Equal => u128::from(a == b),
        Binary::NotEqual => u128::from(a != b),
        Binary::LessSigned => u128::from((a as i128) < (b as i128)),
        Binary::LessUnsigned => u128::from(a < b),
        Binary::LessEqualSigned => u128::from((a as i128) <= (b as i128)),
        Binary::LessEqualUnsigned => u128::from(a <= b),
        Binary::DivideSigned => (a as i128).checked_div(b as i128)? as u128,
        Binary::RemainderSigned => (a as i128).checked_rem(b as i128)? as u128,
        Binary::DivideUnsigned => a.checked_div(b)?,
        Binary::RemainderUnsigned => a.checked_rem(b)?,
    };
    Some(if let ScalarType::Bits(width) = ty {
        value & mask(width)
    } else {
        value
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn modular_comparison_folding_handles_wrap_and_width_boundaries() {
        fn evaluate(graph: &Graph, id: ScalarId, input: u128) -> u128 {
            let scalar = &graph.scalars[id];
            match scalar.operation {
                Operation::Input(_) => input,
                Operation::Constant(value) => value,
                Operation::Binary(op, a, b) => evaluate_binary(
                    op,
                    scalar.ty,
                    evaluate(graph, a, input),
                    evaluate(graph, b, input),
                )
                .unwrap(),
                _ => panic!("unexpected test operation"),
            }
        }
        for width in [0, 1, 8, 32, 64, 128] {
            let ty = ScalarType::Bits(width);
            let mask = mask(width);
            for offset in [0, 1 & mask, mask, mask.saturating_sub(1)] {
                for target in [0, 1 & mask, mask] {
                    let mut graph = Graph::default();
                    let input = graph.input(0, ty, None);
                    let offset_id = graph.constant(ty, offset);
                    let first = graph.binary(ty, Binary::Sub, input, offset_id).unwrap();
                    let second = graph.binary(ty, Binary::Sub, first, offset_id).unwrap();
                    let target_id = graph.constant(ty, target);
                    let condition = graph
                        .binary(ScalarType::Bool, Binary::NotEqual, second, target_id)
                        .unwrap();
                    for value in [0, 1 & mask, mask, mask.saturating_sub(1)] {
                        assert_eq!(
                            evaluate(&graph, condition, value),
                            u128::from(
                                value.wrapping_sub(offset).wrapping_sub(offset) & mask != target
                            ),
                            "width {width} value {value} offset {offset}"
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn constant_extensions_remain_literals_for_fetch_and_decode_specialization() {
        let mut graph = Graph::default();
        let pc = graph.constant(ScalarType::Bits(64), 0x8000_0000);
        let integer = graph
            .unary(ScalarType::Integer, Unary::SignExtend(64), pc)
            .unwrap();
        assert_eq!(graph.literal(integer), Some(0x8000_0000));
        let lower = graph.slice(integer, 0, 32);
        assert_eq!(graph.literal(lower), Some(0x8000_0000));
    }

    #[test]
    fn range_folding_preserves_extended_widths_and_integer_kinds() {
        let mut graph = Graph::default();
        let input = graph.input(0, ScalarType::Bits(8), None);
        let extended = graph
            .unary(ScalarType::Bits(64), Unary::Truncate(64), input)
            .unwrap();
        assert_eq!(graph.scalars[extended].ty, ScalarType::Bits(64));
        assert_eq!(
            graph.scalars[extended].range,
            Some(Range { min: 0, max: 255 })
        );
        let integer = graph
            .unary(ScalarType::Integer, Unary::SignExtend(64), extended)
            .unwrap();
        assert_eq!(graph.scalars[integer].ty, ScalarType::Integer);
        assert_eq!(
            graph.scalars[integer].range,
            Some(Range { min: 0, max: 255 })
        );
    }

    #[test]
    fn fixed_width_identities_still_truncate_wider_operands() {
        let mut graph = Graph::default();
        let input = graph.input(0, ScalarType::Bits(64), None);
        let mask = graph.constant(ScalarType::Bits(8), 255);
        let result = graph
            .binary(ScalarType::Bits(8), Binary::And, input, mask)
            .unwrap();
        assert_ne!(result, input);
        assert_eq!(graph.scalars[result].ty, ScalarType::Bits(8));
    }

    #[test]
    fn repeated_bitfield_writes_reuse_the_same_scalar() {
        for width in [1, 8, 32, 64, 128] {
            let mut graph = Graph::default();
            let ty = ScalarType::Bits(width);
            let input = graph.input(0, ty, None);
            let clear = graph.constant(ty, mask(width) & !0x5a);
            let set = graph.constant(ty, mask(width) & 0x12);
            let cleared = graph.binary(ty, Binary::And, clear, input).unwrap();
            let first = graph.binary(ty, Binary::Or, set, cleared).unwrap();
            let cleared = graph.binary(ty, Binary::And, first, clear).unwrap();
            let second = graph.binary(ty, Binary::Or, cleared, set).unwrap();
            assert_eq!(first, second, "width {width}");
        }
    }

    #[test]
    fn repeated_ticks_collapse_to_one_add() {
        let mut graph = Graph::default();
        let ty = ScalarType::Bits(64);
        let input = graph.input(0, ty, None);
        let one = graph.constant(ty, 1);
        let first = graph.binary(ty, Binary::Add, input, one).unwrap();
        let second = graph.binary(ty, Binary::Add, first, one).unwrap();
        let third = graph.binary(ty, Binary::Add, second, one).unwrap();
        let three = graph.constant(ty, 3);
        assert_eq!(third, graph.binary(ty, Binary::Add, input, three).unwrap());
    }

    #[test]
    fn byte_unpack_and_repack_disappears() {
        let mut graph = Graph::default();
        let input = graph.input(0, ScalarType::Bits(64), None);
        let mut assembled = graph.slice(input, 0, 8);
        for byte in 1..8 {
            let upper = graph.slice(input, byte * 8, 8);
            assembled = graph.concat(upper, assembled, byte * 8, (byte + 1) * 8);
        }
        assert_eq!(assembled, input);
    }

    #[test]
    fn unbounded_integer_overflow_is_not_silently_lowered() {
        let mut graph = Graph::default();
        let input = graph.input(
            0,
            ScalarType::Integer,
            Some(Range {
                min: i128::MAX - 1,
                max: i128::MAX,
            }),
        );
        let one = graph.constant(ScalarType::Integer, 1);
        assert_eq!(
            graph.binary(ScalarType::Integer, Binary::Add, input, one),
            None
        );
        assert_eq!(
            evaluate_binary(Binary::Add, ScalarType::Integer, i128::MAX as u128, 1),
            None
        );
    }

    #[test]
    fn native_width_boundaries_and_oversized_shifts() {
        for width in [0, 1, 8, 32, 64, 127, 128] {
            let ty = ScalarType::Bits(width);
            assert_eq!(evaluate_binary(Binary::Add, ty, mask(width), 1), Some(0));
            assert_eq!(evaluate_binary(Binary::ShiftLeft, ty, 1, 128), Some(0));
            assert_eq!(
                evaluate_unary(Unary::LeadingZeros(width), 0),
                Some(width.into())
            );
        }
        assert_eq!(evaluate_unary(Unary::SignExtend(8), 0xff), Some(u128::MAX));
        assert_eq!(
            evaluate_binary(
                Binary::ArithmeticShiftRight,
                ScalarType::Bits(8),
                0x80,
                1000
            ),
            Some(0xff)
        );
    }
}
