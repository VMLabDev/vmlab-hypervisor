use crate::Value;
use crate::record::{InputSource, Recorded, Shape};
use num_traits::ToPrimitive;
use std::collections::HashMap;
use vmlab_hv_tcg_ir::native::block::{Binding, Recipe};
use vmlab_hv_tcg_ir::native::{Operation, ScalarType, mask};

#[derive(Clone, Copy)]
pub(crate) struct Word {
    pub destination: u32,
    pub source: u32,
    pub scalar: u32,
    pub ty: ScalarType,
}

pub(crate) struct Plan {
    pub slots: Vec<usize>,
    pub words: Vec<Option<Word>>,
}

fn constant(value: &Value) -> Option<(ScalarType, u128)> {
    match value {
        Value::Bool(value) => Some((ScalarType::Bool, u128::from(*value))),
        Value::Bits { width, value } if *width <= 128 => value
            .to_u128()
            .filter(|value| *value <= mask(*width as u16))
            .map(|value| (ScalarType::Bits(*width as u16), value)),
        Value::Integer(value) => value
            .to_i128()
            .map(|value| (ScalarType::Integer, value as u128)),
        _ => None,
    }
}

pub(crate) fn compile(
    recorded: &Recorded,
    destinations: HashMap<usize, Vec<usize>>,
    slots: &[usize],
    scalar_inputs: &[Option<usize>],
    slot_count: usize,
) -> (HashMap<usize, Plan>, Vec<u128>) {
    let bindings: HashMap<usize, &[Binding]> = recorded
        .block
        .exits
        .iter()
        .map(|exit| &exit.globals)
        .chain(
            recorded
                .block
                .checkpoints
                .iter()
                .map(|checkpoint| &checkpoint.globals),
        )
        .map(|bindings| (bindings.as_slice().as_ptr() as usize, bindings.as_slice()))
        .collect();
    let mut constants = Vec::new();
    let mut pool = HashMap::new();
    let plans = destinations.into_iter().map(|(pointer, destinations)| {
        let words = bindings[&pointer].iter().zip(&destinations).map(|(binding, destination)| {
            let (source, scalar, ty) = match recorded.block.values[binding.value] {
                Recipe::Scalar { value, ty } => {
                    if let Operation::Input(input) = recorded.block.graph.scalars[value].operation
                        && matches!(&recorded.inputs[input].source,
                            InputSource::Global(name, path) if name == &binding.name && path.is_empty())
                        && matches!(recorded.inputs[input].shape, Shape::Scalar(expected) if expected == ty)
                    {
                        return Some(Word { destination: u32::MAX, source: 0, scalar: 0, ty });
                    }
                    (slots[value], value, ty)
                }
                Recipe::Input(input) => {
                    let Shape::Scalar(ty) = recorded.inputs[input].shape else { return None; };
                    (scalar_inputs[input]?, usize::MAX, ty)
                }
                Recipe::Constant(id) => {
                    let (ty, raw) = constant(&recorded.constants[id])?;
                    let slot = *pool.entry((ty, raw)).or_insert_with(|| {
                        let slot = slot_count + constants.len();
                        constants.push(raw);
                        slot
                    });
                    (slot, usize::MAX, ty)
                }
                _ => return None,
            };
            Some(Word {
                destination: (*destination).try_into().expect("bounded global slots"),
                source: source.try_into().expect("bounded scalar slots"),
                scalar: if scalar == usize::MAX { u32::MAX } else {
                    scalar.try_into().expect("bounded scalar IDs")
                },
                ty,
            })
        }).collect();
        (pointer, Plan { slots: destinations, words })
    }).collect();
    (plans, constants)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::record::Input;
    use std::sync::Arc;
    use vmlab_hv_tcg_ir::native::block::{Block, Checkpoint};

    #[test]
    fn plans_keep_logical_types_and_intern_constant_words() {
        assert_eq!(std::mem::size_of::<Word>(), 16);
        let mut block = Block::default();
        let input = block.graph.input(0, ScalarType::Bits(64), None);
        let unchanged = block.value(Recipe::Scalar {
            value: input,
            ty: ScalarType::Bits(64),
        });
        let narrowed = block.value(Recipe::Scalar {
            value: input,
            ty: ScalarType::Bits(32),
        });
        let bits = block.value(Recipe::Constant(0));
        let integer = block.value(Recipe::Constant(1));
        let wide = block.value(Recipe::Constant(2));
        let globals = Arc::new(vec![
            Binding {
                name: "same".into(),
                value: unchanged,
            },
            Binding {
                name: "low".into(),
                value: narrowed,
            },
            Binding {
                name: "pc".into(),
                value: bits,
            },
            Binding {
                name: "next".into(),
                value: bits,
            },
            Binding {
                name: "signed".into(),
                value: integer,
            },
            Binding {
                name: "wide".into(),
                value: wide,
            },
        ]);
        let pointer = globals.as_slice().as_ptr() as usize;
        block.checkpoints.push(Checkpoint {
            globals,
            waiting: bits,
            completed_steps: 1,
            fetch_address: None,
        });
        let recorded = Recorded {
            block,
            constants: vec![Value::bits(64, 8u64), Value::int(-1), Value::bits(256, 1u8)],
            inputs: vec![Input {
                source: InputSource::Global("same".into(), vec![]),
                shape: Shape::Scalar(ScalarType::Bits(64)),
                range: None,
            }],
            stop_reason: None,
            dynamic_pc: false,
        };
        let (plans, constants) = compile(
            &recorded,
            HashMap::from([(pointer, (0..6).collect())]),
            &[0],
            &[Some(0)],
            1,
        );
        let plan = &plans[&pointer];
        assert_eq!(plan.words[0].unwrap().destination, u32::MAX);
        assert_eq!(plan.words[1].unwrap().ty, ScalarType::Bits(32));
        assert_eq!(plan.words[1].unwrap().source, 0);
        assert_eq!(plan.words[2].unwrap().source, plan.words[3].unwrap().source);
        assert_eq!(plan.words[4].unwrap().ty, ScalarType::Integer);
        assert!(plan.words[5].is_none());
        assert_eq!(constants, vec![8, u128::MAX]);
    }
}
