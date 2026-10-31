use super::{Binary, Graph, Operation, Range, ScalarId, ScalarType, Unary};

impl Graph {
    pub fn resolve(&self, mut id: ScalarId) -> ScalarId {
        while let Some(next) = self.aliases.get(&id) {
            id = *next;
        }
        id
    }

    fn refine(&mut self, id: ScalarId, lower: Option<i128>, upper: Option<i128>) {
        let id = self.resolve(id);
        let Some(old) = self.scalars[id].range else {
            return;
        };
        let range = Range {
            min: old.min.max(lower.unwrap_or(old.min)),
            max: old.max.min(upper.unwrap_or(old.max)),
        };
        if range == old || range.min > range.max || self.scalars[id].ty == ScalarType::Bool {
            return;
        }
        let next = self.insert(self.scalars[id].ty, Some(range), Operation::Refine(id));
        self.aliases.insert(id, next);
        let mut origin = id;
        while let Operation::Refine(previous) = self.scalars[origin].operation {
            origin = previous;
        }
        if let Operation::Binary(operation, base, constant) = self.scalars[origin].operation
            && self.scalars[base].ty == self.scalars[id].ty
            && let Some(offset) = self.literal(constant)
        {
            let bounds = match self.scalars[id].ty {
                ScalarType::Bits(width)
                    if width < 128 && matches!(operation, Binary::Add | Binary::Sub) =>
                {
                    let convert = |value: i128| {
                        (if operation == Binary::Sub {
                            (value as u128).wrapping_add(offset)
                        } else {
                            (value as u128).wrapping_sub(offset)
                        }) & super::mask(width)
                    };
                    let (min, max) = (convert(range.min), convert(range.max));
                    (min <= max).then_some((min as i128, max as i128))
                }
                ScalarType::Integer if matches!(operation, Binary::Add | Binary::Sub) => {
                    let offset = offset as i128;
                    let convert = |value: i128| {
                        if operation == Binary::Sub {
                            value.checked_add(offset)
                        } else {
                            value.checked_sub(offset)
                        }
                    };
                    convert(range.min).zip(convert(range.max))
                }
                _ => None,
            };
            if let Some((min, max)) = bounds {
                self.refine(base, Some(min), Some(max));
            }
        }
    }

    /// Refine subsequent operations only. The caller must retain a dominating
    /// guard for this condition and outcome, including its precise exit.
    pub fn assume_guard(&mut self, mut condition: ScalarId, mut expected: bool) {
        while let Operation::Unary(Unary::BoolNot, value) = self.scalars[condition].operation {
            condition = value;
            expected = !expected;
        }
        let Operation::Binary(mut op, a, b) = self.scalars[condition].operation else {
            return;
        };
        let (a, b) = (self.resolve(a), self.resolve(b));
        let (Some(left), Some(right)) = (self.scalars[a].range, self.scalars[b].range) else {
            return;
        };
        if matches!(op, Binary::LessUnsigned | Binary::LessEqualUnsigned)
            && (left.min < 0 || right.min < 0)
        {
            return;
        }
        if op == Binary::NotEqual {
            op = Binary::Equal;
            expected = !expected;
        }
        match (op, expected) {
            (Binary::LessSigned | Binary::LessUnsigned, true) => {
                self.refine(a, None, right.max.checked_sub(1));
                self.refine(b, left.min.checked_add(1), None);
            }
            (Binary::LessSigned | Binary::LessUnsigned, false) => {
                self.refine(a, Some(right.min), None);
                self.refine(b, None, Some(left.max));
            }
            (Binary::LessEqualSigned | Binary::LessEqualUnsigned, true) => {
                self.refine(a, None, Some(right.max));
                self.refine(b, Some(left.min), None);
            }
            (Binary::LessEqualSigned | Binary::LessEqualUnsigned, false) => {
                self.refine(a, right.min.checked_add(1), None);
                self.refine(b, None, left.max.checked_sub(1));
            }
            (Binary::Equal, true) => {
                self.refine(a, Some(right.min), Some(right.max));
                self.refine(b, Some(left.min), Some(left.max));
            }
            (Binary::Equal, false) => {
                if right.min == right.max {
                    self.refine(
                        a,
                        (left.min == right.min)
                            .then(|| left.min.checked_add(1))
                            .flatten(),
                        (left.max == right.max)
                            .then(|| left.max.checked_sub(1))
                            .flatten(),
                    );
                }
                if left.min == left.max {
                    self.refine(
                        b,
                        (right.min == left.min)
                            .then(|| right.min.checked_add(1))
                            .flatten(),
                        (right.max == left.max)
                            .then(|| right.max.checked_sub(1))
                            .flatten(),
                    );
                }
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn modular_inverse_bounds_never_exclude_a_value_that_satisfies_the_guard() {
        fn evaluate(graph: &Graph, id: ScalarId, input: u128) -> u128 {
            let scalar = &graph.scalars[id];
            match scalar.operation {
                Operation::Constant(raw) => raw,
                Operation::Input(_) => input,
                Operation::Refine(value) => evaluate(graph, value, input),
                Operation::Binary(operation, a, b) => super::super::evaluate_binary(
                    operation,
                    scalar.ty,
                    evaluate(graph, a, input),
                    evaluate(graph, b, input),
                )
                .unwrap(),
                _ => panic!("unexpected range-test operation"),
            }
        }
        for arithmetic in [Binary::Add, Binary::Sub] {
            for offset in [0, 1, 127, 255] {
                for limit in [0, 1, 127, 255] {
                    for comparison in [
                        Binary::LessUnsigned,
                        Binary::LessEqualUnsigned,
                        Binary::Equal,
                        Binary::NotEqual,
                    ] {
                        for expected in [false, true] {
                            let mut graph = Graph::default();
                            let ty = ScalarType::Bits(8);
                            let input = graph.input(0, ty, None);
                            let offset = graph.constant(ty, offset);
                            let result = graph.binary(ty, arithmetic, input, offset).unwrap();
                            let limit = graph.constant(ty, limit);
                            let condition = graph
                                .binary(ScalarType::Bool, comparison, result, limit)
                                .unwrap();
                            graph.assume_guard(condition, expected);
                            let range = graph.scalars[graph.resolve(input)].range.unwrap();
                            for raw in 0..256 {
                                if (evaluate(&graph, condition, raw) != 0) == expected {
                                    assert!(
                                        range.contains(raw as i128),
                                        "{arithmetic:?} {comparison:?} {expected} value {raw} range {range:?}"
                                    );
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn guarded_ranges_apply_only_to_subsequent_nodes() {
        let mut graph = Graph::default();
        let ty = ScalarType::Bits(64);
        let input = graph.input(0, ty, None);
        let ten = graph.constant(ty, 10);
        let twenty = graph.constant(ty, 20);
        let condition = graph
            .binary(ScalarType::Bool, Binary::LessUnsigned, input, ten)
            .unwrap();
        let before = graph
            .binary(ScalarType::Bool, Binary::LessUnsigned, input, twenty)
            .unwrap();
        graph.assume_guard(condition, true);
        let refined = graph.resolve(input);
        assert_ne!(refined, input);
        assert_eq!(
            graph.scalars[input].range.unwrap().max,
            i128::from(u64::MAX)
        );
        assert_eq!(graph.scalars[refined].range, Some(Range { min: 0, max: 9 }));
        assert_eq!(graph.literal(before), None);
        let after = graph
            .binary(ScalarType::Bool, Binary::LessUnsigned, input, twenty)
            .unwrap();
        assert_eq!(graph.literal(after), Some(1));
    }

    #[test]
    fn refined_effects_retain_the_original_effect_dependency() {
        let mut graph = Graph::default();
        let ty = ScalarType::Bits(8);
        let effect = graph.effect(3, ty);
        let zero = graph.constant(ty, 0);
        let condition = graph
            .binary(ScalarType::Bool, Binary::NotEqual, effect, zero)
            .unwrap();
        graph.assume_guard(condition, true);
        let refined = graph.resolve(effect);
        assert_eq!(graph.scalars[refined].operation, Operation::Refine(effect));
        assert_eq!(
            graph.scalars[refined].range,
            Some(Range { min: 1, max: 255 })
        );
        let quotient = graph
            .binary(ScalarType::Integer, Binary::DivideUnsigned, effect, effect)
            .unwrap();
        assert_eq!(
            graph.scalars[quotient].range,
            Some(Range { min: 0, max: 255 })
        );
    }

    #[test]
    fn unsigned_guards_do_not_interpret_negative_integer_ranges_as_signed() {
        let mut graph = Graph::default();
        let input = graph.input(
            0,
            ScalarType::Integer,
            Some(Range {
                min: -100,
                max: 100,
            }),
        );
        let zero = graph.constant(ScalarType::Integer, 0);
        let condition = graph
            .binary(ScalarType::Bool, Binary::LessUnsigned, input, zero)
            .unwrap();
        graph.assume_guard(condition, false);
        assert_eq!(graph.resolve(input), input);
    }
}
