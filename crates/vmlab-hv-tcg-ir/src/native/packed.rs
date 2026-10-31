//! Compact physical instructions for scalar interpretation.
use super::linear::{Instruction, binary64};
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

impl Packed {
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
        let left = values[self.left as usize];
        let right = || values[self.right as usize];
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
