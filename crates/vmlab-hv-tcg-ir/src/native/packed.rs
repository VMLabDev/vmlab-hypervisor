//! Compact physical instructions for scalar interpretation.
use super::linear::{Guard, Instruction, binary64};
use super::{Binary, ScalarType, Unary, evaluate_binary, evaluate_unary, mask};

#[derive(Clone, Copy, Debug)]
enum Opcode {
    Copy,
    Add64,
    Sub64,
    Mul64,
    And64,
    Or64,
    Xor64,
    AddWord16,
    SubWord16,
    MulWord16,
    AndWord16,
    OrWord16,
    XorWord16,
    AddWord32,
    SubWord32,
    MulWord32,
    AndWord32,
    OrWord32,
    XorWord32,
    AddWord64,
    SubWord64,
    MulWord64,
    AndWord64,
    OrWord64,
    XorWord64,
    AddImmediate64,
    SubImmediate64,
    MulImmediate64,
    AndImmediate64,
    OrImmediate64,
    XorImmediate64,
    AddImmediate16,
    SubImmediate16,
    MulImmediate16,
    AndImmediate16,
    OrImmediate16,
    XorImmediate16,
    AddImmediate32,
    SubImmediate32,
    MulImmediate32,
    AndImmediate32,
    OrImmediate32,
    XorImmediate32,
    ShiftLeftImmediate16,
    ShiftRightImmediate16,
    ArithmeticShiftRightImmediate16,
    ShiftLeftImmediate32,
    ShiftRightImmediate32,
    ArithmeticShiftRightImmediate32,
    ShiftLeftImmediate64,
    ShiftRightImmediate64,
    ArithmeticShiftRightImmediate64,
    Binary(Binary),
    Word(Binary),
    BoolNot,
    BitNot,
    Negate,
    Truncate,
    SignExtend,
    LeadingZeros,
    TrailingZeros,
    Slice,
    Concat,
    Guard(Binary),
    GuardEqual64,
    GuardNotEqual64,
    GuardLess64,
    GuardLessEqual64,
    GuardGreater64,
    GuardGreaterEqual64,
    GuardEqualImmediate64,
    GuardNotEqualImmediate64,
    GuardLessImmediate64,
    GuardLessEqualImmediate64,
    GuardGreaterImmediate64,
    GuardGreaterEqualImmediate64,
    BoolGuard,
    Event,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Control {
    Continue,
    GuardFailed(usize),
    Event(usize),
    InvalidArithmetic,
}

/// All operands name physical slots. Constants and inputs are bound separately.
#[derive(Clone, Copy, Debug)]
pub struct Packed {
    pub output: u32,
    left: u32,
    right: u32,
    opcode: Opcode,
    width: u8,
    extra: u8,
}

#[inline(always)]
fn read_word<const CHECKED: bool>(values: &[u128], index: u32) -> u128 {
    if CHECKED {
        values[index as usize]
    } else {
        // SAFETY: execute_unchecked requires every input index in bounds;
        // all other entry points use checked mode.
        unsafe { *values.get_unchecked(index as usize) }
    }
}

impl Packed {
    pub fn valid_slots(&self, values: usize, stamps: usize) -> bool {
        if self.event_index().is_none()
            && (self.output as usize >= values || self.output as usize >= stamps)
        {
            return false;
        }
        if matches!(self.opcode, Opcode::Event) {
            return true;
        }
        if self.left as usize >= values {
            return false;
        }
        if self.extra == u8::MAX && !matches!(self.opcode, Opcode::Concat) {
            return true;
        }
        match self.opcode {
            Opcode::Copy
            | Opcode::BoolNot
            | Opcode::BitNot
            | Opcode::Negate
            | Opcode::Truncate
            | Opcode::SignExtend
            | Opcode::LeadingZeros
            | Opcode::TrailingZeros
            | Opcode::Slice
            | Opcode::BoolGuard => true,
            _ => (self.right as usize) < values,
        }
    }

    pub fn right_slot(&self) -> Option<usize> {
        matches!(
            self.opcode,
            Opcode::AddWord16
                | Opcode::SubWord16
                | Opcode::MulWord16
                | Opcode::AndWord16
                | Opcode::OrWord16
                | Opcode::XorWord16
                | Opcode::AddWord32
                | Opcode::SubWord32
                | Opcode::MulWord32
                | Opcode::AndWord32
                | Opcode::OrWord32
                | Opcode::XorWord32
                | Opcode::Word(
                    Binary::ShiftLeft | Binary::ShiftRight | Binary::ArithmeticShiftRight
                )
                | Opcode::AddWord64
                | Opcode::SubWord64
                | Opcode::MulWord64
                | Opcode::AndWord64
                | Opcode::OrWord64
                | Opcode::XorWord64
                | Opcode::GuardEqual64
                | Opcode::GuardNotEqual64
                | Opcode::GuardLess64
                | Opcode::GuardLessEqual64
                | Opcode::GuardGreater64
                | Opcode::GuardGreaterEqual64
        )
        .then_some(self.right as usize)
    }

    pub fn with_immediate(mut self, value: u128) -> Option<Self> {
        self.right = value.try_into().ok()?;
        self.opcode = match self.opcode {
            Opcode::AddWord16 => Opcode::AddImmediate16,
            Opcode::SubWord16 => Opcode::SubImmediate16,
            Opcode::MulWord16 => Opcode::MulImmediate16,
            Opcode::AndWord16 => Opcode::AndImmediate16,
            Opcode::OrWord16 => Opcode::OrImmediate16,
            Opcode::XorWord16 => Opcode::XorImmediate16,
            Opcode::AddWord32 => Opcode::AddImmediate32,
            Opcode::SubWord32 => Opcode::SubImmediate32,
            Opcode::MulWord32 => Opcode::MulImmediate32,
            Opcode::AndWord32 => Opcode::AndImmediate32,
            Opcode::OrWord32 => Opcode::OrImmediate32,
            Opcode::XorWord32 => Opcode::XorImmediate32,
            Opcode::Word(Binary::ShiftLeft) => match self.width {
                16 => Opcode::ShiftLeftImmediate16,
                32 => Opcode::ShiftLeftImmediate32,
                64 => Opcode::ShiftLeftImmediate64,
                _ => return None,
            },
            Opcode::Word(Binary::ShiftRight) => match self.width {
                16 => Opcode::ShiftRightImmediate16,
                32 => Opcode::ShiftRightImmediate32,
                64 => Opcode::ShiftRightImmediate64,
                _ => return None,
            },
            Opcode::Word(Binary::ArithmeticShiftRight) => match self.width {
                16 => Opcode::ArithmeticShiftRightImmediate16,
                32 => Opcode::ArithmeticShiftRightImmediate32,
                64 => Opcode::ArithmeticShiftRightImmediate64,
                _ => return None,
            },
            Opcode::AddWord64 => Opcode::AddImmediate64,
            Opcode::SubWord64 => Opcode::SubImmediate64,
            Opcode::MulWord64 => Opcode::MulImmediate64,
            Opcode::AndWord64 => Opcode::AndImmediate64,
            Opcode::OrWord64 => Opcode::OrImmediate64,
            Opcode::XorWord64 => Opcode::XorImmediate64,
            Opcode::GuardEqual64 => Opcode::GuardEqualImmediate64,
            Opcode::GuardNotEqual64 => Opcode::GuardNotEqualImmediate64,
            Opcode::GuardLess64 => Opcode::GuardLessImmediate64,
            Opcode::GuardLessEqual64 => Opcode::GuardLessEqualImmediate64,
            Opcode::GuardGreater64 => Opcode::GuardGreaterImmediate64,
            Opcode::GuardGreaterEqual64 => Opcode::GuardGreaterEqualImmediate64,
            _ => return None,
        };
        self.extra = u8::MAX;
        Some(self)
    }

    pub fn event_index(&self) -> Option<usize> {
        matches!(
            self.opcode,
            Opcode::Event
                | Opcode::Guard(_)
                | Opcode::BoolGuard
                | Opcode::GuardEqual64
                | Opcode::GuardNotEqual64
                | Opcode::GuardLess64
                | Opcode::GuardLessEqual64
                | Opcode::GuardGreater64
                | Opcode::GuardGreaterEqual64
                | Opcode::GuardEqualImmediate64
                | Opcode::GuardNotEqualImmediate64
                | Opcode::GuardLessImmediate64
                | Opcode::GuardLessEqualImmediate64
                | Opcode::GuardGreaterImmediate64
                | Opcode::GuardGreaterEqualImmediate64
        )
        .then_some(self.output as usize)
    }

    pub fn event(event: usize) -> Option<Self> {
        Some(Self {
            output: event.try_into().ok()?,
            left: 0,
            right: 0,
            opcode: Opcode::Event,
            width: 0,
            extra: 0,
        })
    }

    pub fn guard(event: usize, guard: Guard) -> Option<Self> {
        let opcode = if guard.host64 {
            match (guard.operation, guard.expected) {
                (Binary::Equal, true) | (Binary::NotEqual, false) => Opcode::GuardEqual64,
                (Binary::NotEqual, true) | (Binary::Equal, false) => Opcode::GuardNotEqual64,
                (Binary::LessUnsigned, true) => Opcode::GuardLess64,
                (Binary::LessUnsigned, false) => Opcode::GuardGreaterEqual64,
                (Binary::LessEqualUnsigned, true) => Opcode::GuardLessEqual64,
                (Binary::LessEqualUnsigned, false) => Opcode::GuardGreater64,
                _ => Opcode::Guard(guard.operation),
            }
        } else {
            Opcode::Guard(guard.operation)
        };
        Some(Self {
            output: event.try_into().ok()?,
            left: guard.left.try_into().ok()?,
            right: guard.right.try_into().ok()?,
            opcode,
            width: u8::from(guard.host64) | (u8::from(guard.expected) << 1),
            extra: 0,
        })
    }

    pub fn boolean_guard(event: usize, condition: usize, expected: bool) -> Option<Self> {
        Some(Self {
            output: event.try_into().ok()?,
            left: condition.try_into().ok()?,
            right: 0,
            opcode: Opcode::BoolGuard,
            width: u8::from(expected),
            extra: 0,
        })
    }

    #[inline(always)]
    pub fn execute(&self, values: &mut [u128], stamps: &mut [u64], epoch: u64) -> Control {
        self.execute_with::<true>(values, stamps, epoch)
    }

    /// Execute a physical instruction after validating its scratch indices.
    ///
    /// # Safety
    /// `valid_slots(values.len(), stamps.len())` must be true. The instruction
    /// and scratch layout must remain unchanged since validation.
    #[inline(always)]
    pub unsafe fn execute_unchecked(
        &self,
        values: &mut [u128],
        stamps: &mut [u64],
        epoch: u64,
    ) -> Control {
        self.execute_with::<false>(values, stamps, epoch)
    }

    #[inline(always)]
    fn execute_with<const CHECKED: bool>(
        &self,
        values: &mut [u128],
        stamps: &mut [u64],
        epoch: u64,
    ) -> Control {
        let left = || read_word::<CHECKED>(values, self.left);
        let right = || read_word::<CHECKED>(values, self.right);
        let matches = match self.opcode {
            Opcode::Event => return Control::Event(self.output as usize),
            Opcode::Guard(operation) => Guard {
                operation,
                left: 0,
                right: 0,
                expected: self.width & 2 != 0,
                host64: self.width & 1 != 0,
            }
            .matches(left(), right()),
            Opcode::BoolGuard => (left() != 0) == (self.width != 0),
            Opcode::GuardEqual64 => left() as u64 == right() as u64,
            Opcode::GuardNotEqual64 => left() as u64 != right() as u64,
            Opcode::GuardLess64 => (left() as u64) < right() as u64,
            Opcode::GuardLessEqual64 => left() as u64 <= right() as u64,
            Opcode::GuardGreater64 => left() as u64 > right() as u64,
            Opcode::GuardGreaterEqual64 => left() as u64 >= right() as u64,
            Opcode::GuardEqualImmediate64 => left() as u64 == u64::from(self.right),
            Opcode::GuardNotEqualImmediate64 => left() as u64 != u64::from(self.right),
            Opcode::GuardLessImmediate64 => (left() as u64) < u64::from(self.right),
            Opcode::GuardLessEqualImmediate64 => left() as u64 <= u64::from(self.right),
            Opcode::GuardGreaterImmediate64 => left() as u64 > u64::from(self.right),
            Opcode::GuardGreaterEqualImmediate64 => left() as u64 >= u64::from(self.right),
            _ => {
                let Some(value) = self.evaluate_with::<CHECKED>(values) else {
                    return Control::InvalidArithmetic;
                };
                if CHECKED {
                    values[self.output as usize] = value;
                    stamps[self.output as usize] = epoch;
                } else {
                    // SAFETY: execute_unchecked requires both output indices
                    // in bounds for these unchanged scratch slice lengths.
                    unsafe {
                        *values.get_unchecked_mut(self.output as usize) = value;
                        *stamps.get_unchecked_mut(self.output as usize) = epoch;
                    }
                }
                return Control::Continue;
            }
        };
        if matches {
            Control::Continue
        } else {
            Control::GuardFailed(self.output as usize)
        }
    }

    pub fn new(output: usize, instruction: Instruction) -> Option<Self> {
        let mask_width = |bits: u128| {
            let width = bits.trailing_ones() as u8;
            (mask(u16::from(width)) == bits).then_some(width)
        };
        let mut packed = Self {
            output: output.try_into().ok()?,
            left: 0,
            right: 0,
            opcode: Opcode::Copy,
            width: 128,
            extra: 0,
        };
        let (opcode, left, right) = match instruction {
            Instruction::Copy(value) => (Opcode::Copy, value, 0),
            Instruction::Add64(a, b, bits)
            | Instruction::Sub64(a, b, bits)
            | Instruction::Mul64(a, b, bits)
            | Instruction::And64(a, b, bits)
            | Instruction::Or64(a, b, bits)
            | Instruction::Xor64(a, b, bits) => {
                packed.width = mask_width(bits.into())?;
                let opcode = match instruction {
                    Instruction::Add64(..) => Opcode::Add64,
                    Instruction::Sub64(..) => Opcode::Sub64,
                    Instruction::Mul64(..) => Opcode::Mul64,
                    Instruction::And64(..) => Opcode::And64,
                    Instruction::Or64(..) => Opcode::Or64,
                    Instruction::Xor64(..) => Opcode::Xor64,
                    _ => unreachable!(),
                };
                let opcode = match (opcode, bits) {
                    (Opcode::Add64, 0xffff) => Opcode::AddWord16,
                    (Opcode::Sub64, 0xffff) => Opcode::SubWord16,
                    (Opcode::Mul64, 0xffff) => Opcode::MulWord16,
                    (Opcode::And64, 0xffff) => Opcode::AndWord16,
                    (Opcode::Or64, 0xffff) => Opcode::OrWord16,
                    (Opcode::Xor64, 0xffff) => Opcode::XorWord16,
                    (Opcode::Add64, 0xffff_ffff) => Opcode::AddWord32,
                    (Opcode::Sub64, 0xffff_ffff) => Opcode::SubWord32,
                    (Opcode::Mul64, 0xffff_ffff) => Opcode::MulWord32,
                    (Opcode::And64, 0xffff_ffff) => Opcode::AndWord32,
                    (Opcode::Or64, 0xffff_ffff) => Opcode::OrWord32,
                    (Opcode::Xor64, 0xffff_ffff) => Opcode::XorWord32,
                    (Opcode::Add64, u64::MAX) => Opcode::AddWord64,
                    (Opcode::Sub64, u64::MAX) => Opcode::SubWord64,
                    (Opcode::Mul64, u64::MAX) => Opcode::MulWord64,
                    (Opcode::And64, u64::MAX) => Opcode::AndWord64,
                    (Opcode::Or64, u64::MAX) => Opcode::OrWord64,
                    (Opcode::Xor64, u64::MAX) => Opcode::XorWord64,
                    (opcode, _) => opcode,
                };
                (opcode, a, b)
            }
            Instruction::Unary(op, value, bits) => {
                packed.width = mask_width(bits)?;
                let (opcode, extra) = match op {
                    Unary::BoolNot => (Opcode::BoolNot, 0),
                    Unary::BitNot => (Opcode::BitNot, 0),
                    Unary::Negate => (Opcode::Negate, 0),
                    Unary::Truncate(width) => (Opcode::Truncate, width),
                    Unary::SignExtend(width) => (Opcode::SignExtend, width),
                    Unary::LeadingZeros(width) => (Opcode::LeadingZeros, width),
                    Unary::TrailingZeros(width) => (Opcode::TrailingZeros, width),
                };
                packed.extra = extra.try_into().ok()?;
                (opcode, value, 0)
            }
            Instruction::Binary(op, ty, a, b) => {
                packed.width = match ty {
                    ScalarType::Bits(width) => width.try_into().ok()?,
                    ScalarType::Bool => 129,
                    ScalarType::Integer => 130,
                };
                (Opcode::Binary(op), a, b)
            }
            Instruction::Binary64(op, a, b, width) => {
                packed.width = width.try_into().ok()?;
                (Opcode::Word(op), a, b)
            }
            Instruction::Slice(value, offset, bits) => {
                packed.width = mask_width(bits)?;
                packed.extra = offset.try_into().ok()?;
                (Opcode::Slice, value, 0)
            }
            Instruction::Concat(high, low, width, bits) => {
                packed.width = mask_width(bits)?;
                packed.extra = width.try_into().ok()?;
                (Opcode::Concat, high, low)
            }
            Instruction::Constant(_) | Instruction::Input(..) => return None,
        };
        packed.opcode = opcode;
        packed.left = left.try_into().ok()?;
        packed.right = right.try_into().ok()?;
        Some(packed)
    }

    #[inline(always)]
    pub fn evaluate(&self, values: &[u128]) -> Option<u128> {
        self.evaluate_with::<true>(values)
    }

    #[inline(always)]
    fn evaluate_with<const CHECKED: bool>(&self, values: &[u128]) -> Option<u128> {
        if self.event_index().is_some() {
            return None;
        }
        let left = read_word::<CHECKED>(values, self.left);
        let right = || read_word::<CHECKED>(values, self.right);
        let width = u16::from(self.width);
        let extra = u16::from(self.extra);
        let value = match self.opcode {
            Opcode::Copy => left,
            Opcode::Add64 => u128::from((left as u64).wrapping_add(right() as u64)),
            Opcode::Sub64 => u128::from((left as u64).wrapping_sub(right() as u64)),
            Opcode::Mul64 => u128::from((left as u64).wrapping_mul(right() as u64)),
            Opcode::And64 => u128::from(left as u64 & right() as u64),
            Opcode::Or64 => u128::from(left as u64 | right() as u64),
            Opcode::Xor64 => u128::from(left as u64 ^ right() as u64),
            Opcode::AddWord16 => {
                return Some(u128::from((left as u16).wrapping_add(right() as u16)));
            }
            Opcode::SubWord16 => {
                return Some(u128::from((left as u16).wrapping_sub(right() as u16)));
            }
            Opcode::MulWord16 => {
                return Some(u128::from((left as u16).wrapping_mul(right() as u16)));
            }
            Opcode::AndWord16 => return Some(u128::from(left as u16 & right() as u16)),
            Opcode::OrWord16 => return Some(u128::from(left as u16 | right() as u16)),
            Opcode::XorWord16 => return Some(u128::from(left as u16 ^ right() as u16)),
            Opcode::AddWord32 => {
                return Some(u128::from((left as u32).wrapping_add(right() as u32)));
            }
            Opcode::SubWord32 => {
                return Some(u128::from((left as u32).wrapping_sub(right() as u32)));
            }
            Opcode::MulWord32 => {
                return Some(u128::from((left as u32).wrapping_mul(right() as u32)));
            }
            Opcode::AndWord32 => return Some(u128::from(left as u32 & right() as u32)),
            Opcode::OrWord32 => return Some(u128::from(left as u32 | right() as u32)),
            Opcode::XorWord32 => return Some(u128::from(left as u32 ^ right() as u32)),
            Opcode::AddWord64 => {
                return Some(u128::from((left as u64).wrapping_add(right() as u64)));
            }
            Opcode::SubWord64 => {
                return Some(u128::from((left as u64).wrapping_sub(right() as u64)));
            }
            Opcode::MulWord64 => {
                return Some(u128::from((left as u64).wrapping_mul(right() as u64)));
            }
            Opcode::AndWord64 => return Some(u128::from(left as u64 & right() as u64)),
            Opcode::OrWord64 => return Some(u128::from(left as u64 | right() as u64)),
            Opcode::XorWord64 => return Some(u128::from(left as u64 ^ right() as u64)),
            Opcode::AddImmediate64 => {
                return Some(u128::from(
                    (left as u64).wrapping_add(u64::from(self.right)),
                ));
            }
            Opcode::SubImmediate64 => {
                return Some(u128::from(
                    (left as u64).wrapping_sub(u64::from(self.right)),
                ));
            }
            Opcode::MulImmediate64 => {
                return Some(u128::from(
                    (left as u64).wrapping_mul(u64::from(self.right)),
                ));
            }
            Opcode::AndImmediate64 => return Some(u128::from(left as u64 & u64::from(self.right))),
            Opcode::OrImmediate64 => return Some(u128::from(left as u64 | u64::from(self.right))),
            Opcode::XorImmediate64 => return Some(u128::from(left as u64 ^ u64::from(self.right))),
            Opcode::AddImmediate16 => {
                return Some(u128::from((left as u16).wrapping_add(self.right as u16)));
            }
            Opcode::SubImmediate16 => {
                return Some(u128::from((left as u16).wrapping_sub(self.right as u16)));
            }
            Opcode::MulImmediate16 => {
                return Some(u128::from((left as u16).wrapping_mul(self.right as u16)));
            }
            Opcode::AndImmediate16 => return Some(u128::from(left as u16 & self.right as u16)),
            Opcode::OrImmediate16 => return Some(u128::from(left as u16 | self.right as u16)),
            Opcode::XorImmediate16 => return Some(u128::from(left as u16 ^ self.right as u16)),
            Opcode::AddImmediate32 => {
                return Some(u128::from((left as u32).wrapping_add(self.right)));
            }
            Opcode::SubImmediate32 => {
                return Some(u128::from((left as u32).wrapping_sub(self.right)));
            }
            Opcode::MulImmediate32 => {
                return Some(u128::from((left as u32).wrapping_mul(self.right)));
            }
            Opcode::AndImmediate32 => return Some(u128::from(left as u32 & self.right)),
            Opcode::OrImmediate32 => return Some(u128::from(left as u32 | self.right)),
            Opcode::XorImmediate32 => return Some(u128::from(left as u32 ^ self.right)),
            Opcode::ShiftLeftImmediate16 => {
                return Some(u128::from(
                    (left as u16).checked_shl(self.right).unwrap_or(0),
                ));
            }
            Opcode::ShiftRightImmediate16 => {
                return Some(if self.right >= 16 {
                    0
                } else {
                    u128::from(((left as u64) >> self.right) as u16)
                });
            }
            Opcode::ArithmeticShiftRightImmediate16 => {
                return Some(u128::from(((left as i16) >> self.right.min(15)) as u16));
            }
            Opcode::ShiftLeftImmediate32 => {
                return Some(u128::from(
                    (left as u32).checked_shl(self.right).unwrap_or(0),
                ));
            }
            Opcode::ShiftRightImmediate32 => {
                return Some(if self.right >= 32 {
                    0
                } else {
                    u128::from(((left as u64) >> self.right) as u32)
                });
            }
            Opcode::ArithmeticShiftRightImmediate32 => {
                return Some(u128::from(((left as i32) >> self.right.min(31)) as u32));
            }
            Opcode::ShiftLeftImmediate64 => {
                return Some(u128::from(
                    (left as u64).checked_shl(self.right).unwrap_or(0),
                ));
            }
            Opcode::ShiftRightImmediate64 => {
                return Some(u128::from(
                    (left as u64).checked_shr(self.right).unwrap_or(0),
                ));
            }
            Opcode::ArithmeticShiftRightImmediate64 => {
                return Some(u128::from(((left as i64) >> self.right.min(63)) as u64));
            }
            Opcode::Binary(op) => {
                let ty = match self.width {
                    129 => ScalarType::Bool,
                    130 => ScalarType::Integer,
                    _ => ScalarType::Bits(width),
                };
                return evaluate_binary(op, ty, left, right());
            }
            Opcode::Word(op) => return binary64(op, left as u64, right(), width).map(u128::from),
            Opcode::BoolNot => u128::from(left == 0),
            Opcode::BitNot => !left,
            Opcode::Negate => evaluate_unary(Unary::Negate, left)?,
            Opcode::Truncate => left & mask(extra),
            Opcode::SignExtend => evaluate_unary(Unary::SignExtend(extra), left)?,
            Opcode::LeadingZeros => evaluate_unary(Unary::LeadingZeros(extra), left)?,
            Opcode::TrailingZeros => evaluate_unary(Unary::TrailingZeros(extra), left)?,
            Opcode::Slice => left.checked_shr(extra.into()).unwrap_or(0),
            Opcode::Concat => left.checked_shl(extra.into()).unwrap_or(0) | right(),
            Opcode::Guard(_)
            | Opcode::BoolGuard
            | Opcode::Event
            | Opcode::GuardEqual64
            | Opcode::GuardNotEqual64
            | Opcode::GuardLess64
            | Opcode::GuardLessEqual64
            | Opcode::GuardGreater64
            | Opcode::GuardGreaterEqual64 => unreachable!(),
            Opcode::GuardEqualImmediate64
            | Opcode::GuardNotEqualImmediate64
            | Opcode::GuardLessImmediate64
            | Opcode::GuardLessEqualImmediate64
            | Opcode::GuardGreaterImmediate64
            | Opcode::GuardGreaterEqualImmediate64 => unreachable!(),
        };
        Some(value & mask(width))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn physical_instructions_fit_sixteen_bytes() {
        assert_eq!(std::mem::size_of::<Packed>(), 16);
        assert!(Packed::new(0, Instruction::And64(0, 1, 5)).is_none());
    }

    #[test]
    fn packed_controls_leave_values_unchanged_and_preserve_the_event_index() {
        let mut values = [3, 9];
        let mut stamps = [1, 1];
        let guard = Packed::guard(
            7,
            Guard {
                operation: Binary::LessUnsigned,
                left: 0,
                right: 1,
                expected: true,
                host64: true,
            },
        )
        .unwrap();
        assert_eq!(
            guard.execute(&mut values, &mut stamps, 2),
            Control::Continue
        );
        values[0] = 10;
        assert_eq!(
            guard.execute(&mut values, &mut stamps, 2),
            Control::GuardFailed(7)
        );
        assert_eq!(
            Packed::event(11)
                .unwrap()
                .execute(&mut values, &mut stamps, 2),
            Control::Event(11)
        );
        assert_eq!(values, [10, 9]);
        assert_eq!(stamps, [1, 1]);
    }

    #[test]
    fn scratch_validation_rejects_bad_inputs_and_outputs() {
        let mut word = Packed::new(2, Instruction::Add64(0, 1, u64::MAX)).unwrap();
        assert!(word.valid_slots(3, 3));
        assert!(!word.valid_slots(2, 3));
        assert!(!word.valid_slots(3, 2));
        word.output = 3;
        assert!(!word.valid_slots(3, 3));
        assert!(Packed::event(7).unwrap().valid_slots(0, 0));
        let concat = Packed::new(0, Instruction::Concat(0, 1, 255, u128::MAX)).unwrap();
        assert!(!concat.valid_slots(1, 1));
    }

    fn compare_execution(packed: Packed, left: u128, right: u128) {
        let mut checked = [left, right, 99];
        let mut unchecked = checked;
        let mut checked_stamps = [1, 1, 1];
        let mut unchecked_stamps = checked_stamps;
        assert!(packed.valid_slots(checked.len(), checked_stamps.len()));
        let expected = packed.execute(&mut checked, &mut checked_stamps, 7);
        // SAFETY: valid_slots above covers these fixed arrays and instruction.
        let actual = unsafe { packed.execute_unchecked(&mut unchecked, &mut unchecked_stamps, 7) };
        assert_eq!(actual, expected);
        assert_eq!(unchecked, checked);
        assert_eq!(unchecked_stamps, checked_stamps);
    }

    #[test]
    fn validated_execution_matches_checked_execution() {
        for width in [0, 1, 8, 16, 32, 64, 128] {
            for left in [0, 1, u64::MAX as u128, u128::MAX] {
                for right in [0, 1, 31, 64, 128, u32::MAX as u128, u128::MAX] {
                    for op in [
                        Binary::Add,
                        Binary::Sub,
                        Binary::Mul,
                        Binary::And,
                        Binary::Or,
                        Binary::Xor,
                        Binary::ShiftLeft,
                        Binary::ShiftRight,
                        Binary::ArithmeticShiftRight,
                        Binary::Equal,
                        Binary::NotEqual,
                        Binary::LessSigned,
                        Binary::LessUnsigned,
                        Binary::LessEqualSigned,
                        Binary::LessEqualUnsigned,
                        Binary::DivideSigned,
                        Binary::RemainderSigned,
                        Binary::DivideUnsigned,
                        Binary::RemainderUnsigned,
                    ] {
                        let packed =
                            Packed::new(2, Instruction::Binary(op, ScalarType::Bits(width), 0, 1))
                                .unwrap();
                        compare_execution(packed, left, right);
                        if width <= 64
                            && matches!(
                                op,
                                Binary::Add
                                    | Binary::Sub
                                    | Binary::Mul
                                    | Binary::And
                                    | Binary::Or
                                    | Binary::Xor
                                    | Binary::ShiftLeft
                                    | Binary::ShiftRight
                                    | Binary::ArithmeticShiftRight
                            )
                        {
                            let packed =
                                Packed::new(2, Instruction::Binary64(op, 0, 1, width)).unwrap();
                            compare_execution(packed, left, right);
                            if let Some(immediate) = packed.with_immediate(right) {
                                compare_execution(immediate, left, right);
                            }
                        }
                    }
                    for operation in [
                        Binary::Equal,
                        Binary::NotEqual,
                        Binary::LessUnsigned,
                        Binary::LessEqualUnsigned,
                    ] {
                        for expected in [false, true] {
                            let packed = Packed::guard(
                                7,
                                Guard {
                                    operation,
                                    left: 0,
                                    right: 1,
                                    expected,
                                    host64: true,
                                },
                            )
                            .unwrap();
                            compare_execution(packed, left, right);
                            if let Some(immediate) = packed.with_immediate(right) {
                                compare_execution(immediate, left, right);
                            }
                        }
                    }
                    compare_execution(Packed::boolean_guard(7, 0, true).unwrap(), left, right);
                    compare_execution(Packed::event(7).unwrap(), left, right);
                }
                for op in [
                    Unary::BoolNot,
                    Unary::BitNot,
                    Unary::Negate,
                    Unary::Truncate(width),
                    Unary::SignExtend(width),
                    Unary::LeadingZeros(width),
                    Unary::TrailingZeros(width),
                ] {
                    compare_execution(
                        Packed::new(2, Instruction::Unary(op, 0, mask(width))).unwrap(),
                        left,
                        0,
                    );
                }
                for offset in [0, 1, 63, 64, 127, 128, 255] {
                    compare_execution(
                        Packed::new(2, Instruction::Slice(0, offset, mask(width))).unwrap(),
                        left,
                        0,
                    );
                    compare_execution(
                        Packed::new(2, Instruction::Concat(0, 1, offset, mask(width))).unwrap(),
                        left,
                        1,
                    );
                }
            }
        }
    }

    #[test]
    fn embedded_constants_preserve_words_and_oversized_shift_counts() {
        for width in [16, 32, 64] {
            for left in [0, 1, u32::MAX as u128, u64::MAX as u128, u128::MAX] {
                for right in [0, 1, 15, 16, 31, 32, 63, 64, 128, u32::MAX as u128] {
                    for op in [
                        Binary::Add,
                        Binary::Sub,
                        Binary::Mul,
                        Binary::And,
                        Binary::Or,
                        Binary::Xor,
                        Binary::ShiftLeft,
                        Binary::ShiftRight,
                        Binary::ArithmeticShiftRight,
                    ] {
                        let instruction = if matches!(
                            op,
                            Binary::ShiftLeft | Binary::ShiftRight | Binary::ArithmeticShiftRight
                        ) {
                            Instruction::Binary64(op, 0, 1, width)
                        } else {
                            let bits = mask(width) as u64;
                            match op {
                                Binary::Add => Instruction::Add64(0, 1, bits),
                                Binary::Sub => Instruction::Sub64(0, 1, bits),
                                Binary::Mul => Instruction::Mul64(0, 1, bits),
                                Binary::And => Instruction::And64(0, 1, bits),
                                Binary::Or => Instruction::Or64(0, 1, bits),
                                Binary::Xor => Instruction::Xor64(0, 1, bits),
                                _ => unreachable!(),
                            }
                        };
                        let packed = Packed::new(2, instruction)
                            .unwrap()
                            .with_immediate(right)
                            .unwrap();
                        assert_eq!(
                            packed.evaluate(&[left]),
                            binary64(op, left as u64, right, width).map(u128::from),
                            "{op:?} width {width} left {left} right {right}"
                        );
                    }
                }
            }
        }
        for op in [
            Binary::Equal,
            Binary::NotEqual,
            Binary::LessUnsigned,
            Binary::LessEqualUnsigned,
        ] {
            for expected in [false, true] {
                for right in [0, 1, u32::MAX as u128] {
                    let guard = Guard {
                        operation: op,
                        left: 0,
                        right: 1,
                        expected,
                        host64: true,
                    };
                    let packed = Packed::guard(7, guard)
                        .unwrap()
                        .with_immediate(right)
                        .unwrap();
                    for left in [0, 1, u64::MAX as u128] {
                        assert_eq!(
                            packed.execute(&mut [left], &mut [1], 2),
                            if guard.matches(left, right) {
                                Control::Continue
                            } else {
                                Control::GuardFailed(7)
                            }
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn specialized_word_widths_and_guards_match_the_generic_operations() {
        for width in [0, 1, 8, 16, 32, 63, 64] {
            for left in [0, 1, u32::MAX as u128, u64::MAX as u128, u128::MAX] {
                for right in [0, 1, u32::MAX as u128, u64::MAX as u128, u128::MAX] {
                    for op in [
                        Binary::Add,
                        Binary::Sub,
                        Binary::Mul,
                        Binary::And,
                        Binary::Or,
                        Binary::Xor,
                    ] {
                        let bits = mask(width) as u64;
                        let instruction = match op {
                            Binary::Add => Instruction::Add64(0, 1, bits),
                            Binary::Sub => Instruction::Sub64(0, 1, bits),
                            Binary::Mul => Instruction::Mul64(0, 1, bits),
                            Binary::And => Instruction::And64(0, 1, bits),
                            Binary::Or => Instruction::Or64(0, 1, bits),
                            Binary::Xor => Instruction::Xor64(0, 1, bits),
                            _ => unreachable!(),
                        };
                        assert_eq!(
                            Packed::new(2, instruction)
                                .unwrap()
                                .evaluate(&[left, right]),
                            evaluate_binary(
                                op,
                                ScalarType::Bits(width),
                                left as u64 as u128,
                                right as u64 as u128
                            )
                        );
                    }
                    for operation in [
                        Binary::Equal,
                        Binary::NotEqual,
                        Binary::LessUnsigned,
                        Binary::LessEqualUnsigned,
                    ] {
                        for expected in [false, true] {
                            let guard = Guard {
                                operation,
                                left: 0,
                                right: 1,
                                expected,
                                host64: true,
                            };
                            let packed = Packed::guard(7, guard).unwrap();
                            let outcome = packed.execute(&mut [left, right], &mut [1, 1], 2);
                            assert_eq!(
                                outcome,
                                if guard.matches(left, right) {
                                    Control::Continue
                                } else {
                                    Control::GuardFailed(7)
                                }
                            );
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn packed_words_keep_full_shift_counts_and_logical_widths() {
        for width in [0, 1, 8, 32, 64] {
            for left in [0, 1, u32::MAX as u128, u64::MAX as u128] {
                let left = left & mask(width);
                for right in [0, 1, 63, 64, 128, u128::MAX] {
                    for op in [
                        Binary::Add,
                        Binary::Sub,
                        Binary::Mul,
                        Binary::And,
                        Binary::Or,
                        Binary::Xor,
                        Binary::ShiftLeft,
                        Binary::ShiftRight,
                        Binary::ArithmeticShiftRight,
                    ] {
                        let packed =
                            Packed::new(2, Instruction::Binary64(op, 0, 1, width)).unwrap();
                        assert_eq!(
                            packed.evaluate(&[left, right]),
                            evaluate_binary(op, ScalarType::Bits(width), left, right)
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn packed_generic_operations_keep_integer_overflow_and_wide_values() {
        let values = [
            0,
            1,
            u64::MAX as u128,
            i128::MAX as u128,
            i128::MIN as u128,
            u128::MAX,
        ];
        for width in [0, 1, 8, 64, 127, 128] {
            for a in values {
                for b in values {
                    for ty in [
                        ScalarType::Bits(width),
                        ScalarType::Integer,
                        ScalarType::Bool,
                    ] {
                        for op in [
                            Binary::Add,
                            Binary::Sub,
                            Binary::Mul,
                            Binary::And,
                            Binary::Or,
                            Binary::Xor,
                            Binary::ShiftLeft,
                            Binary::ShiftRight,
                            Binary::ArithmeticShiftRight,
                            Binary::Equal,
                            Binary::NotEqual,
                            Binary::LessSigned,
                            Binary::LessUnsigned,
                            Binary::LessEqualSigned,
                            Binary::LessEqualUnsigned,
                            Binary::DivideSigned,
                            Binary::RemainderSigned,
                            Binary::DivideUnsigned,
                            Binary::RemainderUnsigned,
                        ] {
                            let instruction =
                                Packed::new(2, Instruction::Binary(op, ty, 0, 1)).unwrap();
                            assert_eq!(
                                instruction.evaluate(&[a, b]),
                                evaluate_binary(op, ty, a, b)
                            );
                        }
                    }
                }
                for op in [
                    Unary::BoolNot,
                    Unary::BitNot,
                    Unary::Negate,
                    Unary::Truncate(width),
                    Unary::SignExtend(width),
                    Unary::LeadingZeros(width),
                    Unary::TrailingZeros(width),
                ] {
                    let instruction =
                        Packed::new(2, Instruction::Unary(op, 0, mask(width))).unwrap();
                    assert_eq!(
                        instruction.evaluate(&[a]),
                        evaluate_unary(op, a).map(|value| value & mask(width))
                    );
                }
                for offset in [0, 1, 63, 64, 127, 128] {
                    let instruction =
                        Packed::new(2, Instruction::Slice(0, offset, mask(width))).unwrap();
                    assert_eq!(
                        instruction.evaluate(&[a]),
                        Some(a.checked_shr(offset.into()).unwrap_or(0) & mask(width))
                    );
                    for b in values {
                        let instruction =
                            Packed::new(2, Instruction::Concat(0, 1, offset, mask(width))).unwrap();
                        assert_eq!(
                            instruction.evaluate(&[a, b]),
                            Some((a.checked_shl(offset.into()).unwrap_or(0) | b) & mask(width))
                        );
                    }
                }
            }
        }
    }
}
