use super::{Binary, Graph, Operation, ScalarId, ScalarType, Unary, mask};

#[derive(Clone, Copy, Debug)]
pub struct Guard {
    pub operation: Binary,
    pub left: ScalarId,
    pub right: ScalarId,
    pub expected: bool,
    pub host64: bool,
}

pub fn guard(graph: &Graph, mut condition: ScalarId, mut expected: bool) -> Option<Guard> {
    while let Operation::Unary(Unary::BoolNot, value) = graph.scalars[condition].operation {
        condition = value;
        expected = !expected;
    }
    let Operation::Binary(mut operation, left, right) = graph.scalars[condition].operation else {
        return None;
    };
    if !matches!(
        operation,
        Binary::Equal
            | Binary::NotEqual
            | Binary::LessSigned
            | Binary::LessUnsigned
            | Binary::LessEqualSigned
            | Binary::LessEqualUnsigned
    ) {
        return None;
    }
    let host64 = [left, right].into_iter().all(|id| {
        graph.scalars[id]
            .range
            .is_some_and(|range| range.min >= 0 && range.max <= i128::from(u64::MAX))
    });
    if host64 {
        operation = match operation {
            Binary::LessSigned => Binary::LessUnsigned,
            Binary::LessEqualSigned => Binary::LessEqualUnsigned,
            op => op,
        };
    }
    Some(Guard {
        operation,
        left,
        right,
        expected,
        host64,
    })
}

impl Guard {
    #[inline(always)]
    pub fn matches(self, left: u128, right: u128) -> bool {
        let value = if self.host64 {
            let (left, right) = (left as u64, right as u64);
            match self.operation {
                Binary::Equal => left == right,
                Binary::NotEqual => left != right,
                Binary::LessUnsigned => left < right,
                Binary::LessEqualUnsigned => left <= right,
                _ => unreachable!("host word comparison"),
            }
        } else {
            super::evaluate_binary(self.operation, ScalarType::Bool, left, right)
                .expect("total comparison")
                != 0
        };
        value == self.expected
    }
}

#[derive(Clone, Debug)]
pub enum Instruction {
    Copy(ScalarId),
    Constant(u128),
    Input(usize, ScalarType),
    Unary(Unary, ScalarId, u128),
    Binary(Binary, ScalarType, ScalarId, ScalarId),
    Add64(ScalarId, ScalarId, u64),
    Sub64(ScalarId, ScalarId, u64),
    Mul64(ScalarId, ScalarId, u64),
    And64(ScalarId, ScalarId, u64),
    Or64(ScalarId, ScalarId, u64),
    Xor64(ScalarId, ScalarId, u64),
    /// A fixed-width result needs only the low host word. Shift counts retain
    /// their full input width, including negative integer modulo operands.
    Binary64(Binary, ScalarId, ScalarId, u16),
    Slice(ScalarId, u16, u128),
    Concat(ScalarId, ScalarId, u16, u128),
}

impl Instruction {
    pub fn remap(&mut self, slots: &[usize]) {
        match self {
            Self::Copy(value) | Self::Unary(_, value, _) | Self::Slice(value, ..) => {
                *value = slots[*value]
            }
            Self::Binary(_, _, a, b)
            | Self::Binary64(_, a, b, _)
            | Self::Add64(a, b, _)
            | Self::Sub64(a, b, _)
            | Self::Mul64(a, b, _)
            | Self::And64(a, b, _)
            | Self::Or64(a, b, _)
            | Self::Xor64(a, b, _)
            | Self::Concat(a, b, ..) => {
                *a = slots[*a];
                *b = slots[*b];
            }
            Self::Constant(_) | Self::Input(..) => {}
        }
    }
}

/// Coalesce range and type views while retaining their logical SSA metadata.
pub fn slots(graph: &Graph) -> (Vec<usize>, usize) {
    let mut slots = Vec::with_capacity(graph.scalars.len());
    let mut next = 0;
    for scalar in &graph.scalars {
        if let Operation::Refine(value) = scalar.operation {
            slots.push(slots[value]);
        } else {
            slots.push(next);
            next += 1;
        }
    }
    (slots, next)
}

pub fn lower(graph: &Graph, id: ScalarId) -> Option<Instruction> {
    let scalar = &graph.scalars[id];
    let word_mask = match scalar.ty {
        ScalarType::Bits(width) if width <= 64 => Some(mask(width) as u64),
        _ if scalar
            .range
            .is_some_and(|range| range.min >= 0 && range.max <= i128::from(u64::MAX)) =>
        {
            Some(u64::MAX)
        }
        _ => None,
    };
    if let Operation::Binary(operation, a, b) = scalar.operation
        && let Some(mask) = word_mask
    {
        let word = match operation {
            Binary::Add => Some(Instruction::Add64(a, b, mask)),
            Binary::Sub => Some(Instruction::Sub64(a, b, mask)),
            Binary::Mul => Some(Instruction::Mul64(a, b, mask)),
            Binary::And => Some(Instruction::And64(a, b, mask)),
            Binary::Or => Some(Instruction::Or64(a, b, mask)),
            Binary::Xor => Some(Instruction::Xor64(a, b, mask)),
            _ => None,
        };
        if word.is_some() {
            return word;
        }
    }
    let bits = match scalar.ty {
        ScalarType::Bits(width) => mask(width),
        _ => u128::MAX,
    };
    Some(match scalar.operation {
        Operation::Refine(value) => Instruction::Copy(value),
        Operation::Constant(value) => Instruction::Constant(value),
        Operation::Input(input) => Instruction::Input(input, scalar.ty),
        Operation::Effect(_) => return None,
        Operation::Unary(operation, value) => Instruction::Unary(operation, value, bits),
        Operation::Binary(operation, a, b) => match scalar.ty {
            ScalarType::Bits(width)
                if width <= 64
                    && (operation != Binary::ShiftRight
                        || matches!(graph.scalars[a].ty, ScalarType::Bits(actual) if actual <= width)
                        || graph.scalars[a].range.is_some_and(|range| {
                            range.min >= 0 && range.max as u128 <= mask(width)
                        }))
                    && matches!(
                        operation,
                        Binary::Add
                            | Binary::Sub
                            | Binary::Mul
                            | Binary::And
                            | Binary::Or
                            | Binary::Xor
                            | Binary::ShiftLeft
                            | Binary::ShiftRight
                            | Binary::ArithmeticShiftRight
                    ) =>
            {
                Instruction::Binary64(operation, a, b, width)
            }
            _ => Instruction::Binary(operation, scalar.ty, a, b),
        },
        Operation::Slice {
            value,
            offset,
            width,
        } => Instruction::Slice(value, offset, mask(width)),
        Operation::Concat {
            high,
            low,
            low_width,
        } => Instruction::Concat(high, low, low_width, bits),
    })
}

pub fn binary64(operation: Binary, a: u64, b: u128, width: u16) -> Option<u64> {
    let result = match operation {
        Binary::Add => a.wrapping_add(b as u64),
        Binary::Sub => a.wrapping_sub(b as u64),
        Binary::Mul => a.wrapping_mul(b as u64),
        Binary::And => a & b as u64,
        Binary::Or => a | b as u64,
        Binary::Xor => a ^ b as u64,
        Binary::ShiftLeft => {
            if b >= u128::from(width) {
                0
            } else {
                a << b
            }
        }
        Binary::ShiftRight => {
            if b >= u128::from(width) {
                0
            } else {
                a >> b
            }
        }
        Binary::ArithmeticShiftRight => {
            if width == 0 {
                0
            } else {
                let signed = ((a << (64 - width)) as i64) >> (64 - width);
                (signed >> b.min(u128::from(width - 1))) as u64
            }
        }
        _ => return None,
    };
    Some(result & mask(width) as u64)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_narrow_logical_right_shift_keeps_wider_input_bits() {
        for (source_width, width, value, count, expected) in [
            (64, 8, 1u128 << 20, 20, 1),
            (128, 64, 1u128 << 70, 16, 1u128 << 54),
        ] {
            let mut graph = Graph::default();
            let input = graph.input(0, ScalarType::Bits(source_width), None);
            let amount = graph.constant(ScalarType::Integer, count);
            let shifted = graph
                .binary(ScalarType::Bits(width), Binary::ShiftRight, input, amount)
                .unwrap();
            let instruction = lower(&graph, shifted).unwrap();
            assert!(matches!(
                instruction,
                Instruction::Binary(Binary::ShiftRight, _, _, _)
            ));
            let packed = super::super::packed::Packed::new(2, instruction).unwrap();
            assert_eq!(packed.evaluate(&[value, count]), Some(expected));
        }
    }

    #[test]
    fn logical_views_share_a_slot_and_preserve_their_types() {
        let mut graph = Graph::default();
        let byte = graph.input(0, ScalarType::Bits(8), None);
        let word = graph
            .unary(ScalarType::Bits(64), Unary::Truncate(64), byte)
            .unwrap();
        let integer = graph
            .unary(ScalarType::Integer, Unary::SignExtend(64), word)
            .unwrap();
        let one = graph.constant(ScalarType::Integer, 1);
        let sum = graph
            .binary(ScalarType::Integer, Binary::Add, integer, one)
            .unwrap();
        let (slots, count) = slots(&graph);
        assert_eq!(slots[byte], slots[word]);
        assert_eq!(slots[word], slots[integer]);
        assert_eq!(graph.scalars[word].ty, ScalarType::Bits(64));
        assert_eq!(graph.scalars[integer].ty, ScalarType::Integer);
        assert_eq!(count, 3);
        let mut instruction = lower(&graph, sum).unwrap();
        instruction.remap(&slots);
        assert!(
            matches!(instruction, Instruction::Add64(a, b, u64::MAX) if a == slots[byte] && b == slots[one])
        );
    }

    #[test]
    fn scalar_host_words_match_u128_semantics_including_large_shift_counts() {
        for width in [0, 1, 8, 32, 63, 64] {
            for a in [0, 1, u32::MAX as u64, u64::MAX] {
                let a = a & mask(width) as u64;
                for b in [0, 1, 63, 64, 128, u64::MAX as u128, u128::MAX] {
                    for operation in [
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
                        assert_eq!(
                            binary64(operation, a, b, width).map(u128::from),
                            super::super::evaluate_binary(
                                operation,
                                ScalarType::Bits(width),
                                a.into(),
                                b
                            ),
                            "{operation:?} {width} {a} {b}"
                        );
                    }
                }
            }
        }
    }
}
