use super::{Binary, Graph, Operation, ScalarId, ScalarType, Unary, mask};

enum Bit {
    Constant(bool),
    Source(ScalarId, u16),
}

impl Graph {
    fn bit(&self, id: ScalarId, index: u16, depth: u8) -> Option<Bit> {
        if depth == 64 {
            return None;
        }
        if index >= 128 || matches!(self.scalars[id].ty, ScalarType::Bits(width) if index >= width)
        {
            return Some(Bit::Constant(false));
        }
        match self.scalars[id].operation {
            Operation::Constant(value) => Some(Bit::Constant(value & (1u128 << index) != 0)),
            Operation::Refine(value) => self.bit(value, index, depth + 1),
            Operation::Unary(Unary::Truncate(width), value) => {
                if index < width {
                    self.bit(value, index, depth + 1)
                } else {
                    Some(Bit::Constant(false))
                }
            }
            Operation::Slice {
                value,
                offset,
                width,
            } => {
                if index >= width {
                    Some(Bit::Constant(false))
                } else {
                    self.bit(value, offset + index, depth + 1)
                }
            }
            Operation::Concat {
                high,
                low,
                low_width,
            } => {
                if index < low_width {
                    self.bit(low, index, depth + 1)
                } else {
                    self.bit(high, index - low_width, depth + 1)
                }
            }
            Operation::Binary(operation, value, constant)
                if matches!(operation, Binary::And | Binary::Or | Binary::Xor)
                    && self.literal(constant).is_some() =>
            {
                let set = self.literal(constant)? & (1u128 << index) != 0;
                match (operation, set) {
                    (Binary::And, false) => Some(Bit::Constant(false)),
                    (Binary::Or, true) => Some(Bit::Constant(true)),
                    (Binary::Xor, true) => match self.bit(value, index, depth + 1)? {
                        Bit::Constant(value) => Some(Bit::Constant(!value)),
                        _ => None,
                    },
                    _ => self.bit(value, index, depth + 1),
                }
            }
            _ => Some(Bit::Source(id, index)),
        }
    }

    pub(super) fn fold_bitfield_concat(
        &mut self,
        high: ScalarId,
        low: ScalarId,
        low_width: u16,
        width: u16,
    ) -> Option<ScalarId> {
        if !matches!(self.scalars[low].ty, ScalarType::Bits(bits) if bits <= low_width)
            && !self.scalars[low]
                .range
                .is_some_and(|range| range.min >= 0 && range.max as u128 <= mask(low_width))
        {
            return None;
        }
        let mut base = None;
        let mut keep = 0;
        let mut literal = 0;
        for index in 0..width {
            let bit = if index < low_width {
                self.bit(low, index, 0)?
            } else {
                self.bit(high, index - low_width, 0)?
            };
            match bit {
                Bit::Constant(true) => literal |= 1u128 << index,
                Bit::Constant(false) => {}
                Bit::Source(id, source)
                    if source == index && base.is_none_or(|base| base == id) =>
                {
                    base = Some(id);
                    keep |= 1u128 << index;
                }
                _ => return None,
            }
        }
        let ty = ScalarType::Bits(width);
        let Some(base) = base else {
            return Some(self.constant(ty, literal));
        };
        if self.scalars[base].ty != ty {
            return None;
        }
        let keep = self.constant(ty, keep);
        let preserved = self.binary(ty, Binary::And, base, keep)?;
        let literal = self.constant(ty, literal);
        self.binary(ty, Binary::Or, preserved, literal)
    }

    pub(super) fn fold_masked_comparison(
        &mut self,
        operation: Binary,
        left: ScalarId,
        right: ScalarId,
    ) -> Option<ScalarId> {
        for (base, updated) in [(left, right), (right, left)] {
            let ScalarType::Bits(width) = self.scalars[base].ty else {
                continue;
            };
            if self.scalars[updated].ty != self.scalars[base].ty {
                continue;
            }
            let (preserved, literal) = match self.scalars[updated].operation {
                Operation::Binary(Binary::Or, preserved, constant) => {
                    (preserved, self.literal(constant)?)
                }
                _ => (updated, 0),
            };
            if let Operation::Binary(Binary::And, source, constant) =
                self.scalars[preserved].operation
                && source == base
                && let Some(keep) = self.literal(constant)
                && literal & keep == 0
                && literal <= mask(width)
            {
                let ty = ScalarType::Bits(width);
                let changed = self.constant(ty, !keep & mask(width));
                let actual = self.binary(ty, Binary::And, base, changed)?;
                let expected = self.constant(ty, literal);
                return self.binary(ScalarType::Bool, operation, actual, expected);
            }
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn evaluate(graph: &Graph, id: ScalarId, input: u128) -> u128 {
        let scalar = &graph.scalars[id];
        match scalar.operation {
            Operation::Input(_) => input,
            Operation::Constant(value) => value,
            Operation::Refine(value) => evaluate(graph, value, input),
            Operation::Binary(operation, a, b) => super::super::evaluate_binary(
                operation,
                scalar.ty,
                evaluate(graph, a, input),
                evaluate(graph, b, input),
            )
            .unwrap(),
            Operation::Slice {
                value,
                offset,
                width,
            } => {
                evaluate(graph, value, input)
                    .checked_shr(offset.into())
                    .unwrap_or(0)
                    & mask(width)
            }
            Operation::Concat {
                high,
                low,
                low_width,
            } => {
                (evaluate(graph, high, input)
                    .checked_shl(low_width.into())
                    .unwrap_or(0)
                    | evaluate(graph, low, input))
                    & match scalar.ty {
                        ScalarType::Bits(width) => mask(width),
                        _ => u128::MAX,
                    }
            }
            _ => panic!("unexpected bitfield operation"),
        }
    }

    #[test]
    fn concatenated_bitfield_writes_and_self_comparisons_preserve_every_bit() {
        for width in [1, 8, 32, 64, 128] {
            for offset in [0, width / 2, width - 1] {
                let mut graph = Graph::default();
                let ty = ScalarType::Bits(width);
                let input = graph.input(0, ty, None);
                let low = graph.slice(input, 0, offset);
                let high = graph.slice(input, offset + 1, width - offset - 1);
                let one = graph.constant(ScalarType::Bits(1), 1);
                let lower = graph.concat(one, low, offset, offset + 1);
                let updated = graph.concat(high, lower, offset + 1, width);
                let differs = graph
                    .binary(ScalarType::Bool, Binary::NotEqual, input, updated)
                    .unwrap();
                for raw in [0, 1, mask(width), mask(width) & !0x5a, mask(width) >> 1] {
                    let expected = raw | (1u128 << offset);
                    assert_eq!(
                        evaluate(&graph, updated, raw),
                        expected,
                        "width {width} offset {offset}"
                    );
                    assert_eq!(evaluate(&graph, differs, raw), u128::from(raw != expected));
                }
            }
        }
    }
}
