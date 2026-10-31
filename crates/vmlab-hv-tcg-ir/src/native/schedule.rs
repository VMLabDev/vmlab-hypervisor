use super::block::{Block, Event, Recipe, ValueId};
use super::{Graph, Operation, ScalarId};

/// Pure scalar dependencies scheduled before each ordered effect. Exit-only
/// values stay lazy, so precise continuations do not burden the successful path.
fn visit(graph: &Graph, id: ScalarId, seen: &mut [bool], output: &mut Vec<ScalarId>) {
    if seen[id] {
        return;
    }
    seen[id] = true;
    match graph.scalars[id].operation {
        Operation::Refine(value) | Operation::Unary(_, value) | Operation::Slice { value, .. } => {
            visit(graph, value, seen, output)
        }
        Operation::Binary(_, a, b)
        | Operation::Concat {
            high: a, low: b, ..
        } => {
            visit(graph, a, seen, output);
            visit(graph, b, seen, output);
        }
        Operation::Effect(_) => return,
        _ => {}
    }
    output.push(id);
}
pub fn schedule(block: &Block) -> Vec<Vec<ScalarId>> {
    let mut seen = vec![false; block.graph.scalars.len()];
    block
        .events
        .iter()
        .map(|event| {
            let mut output = Vec::new();
            match *event {
                Event::Guard {
                    condition,
                    expected,
                    ..
                } => {
                    if let Some(guard) = super::linear::guard(&block.graph, condition, expected) {
                        visit(&block.graph, guard.left, &mut seen, &mut output);
                        visit(&block.graph, guard.right, &mut seen, &mut output);
                    } else {
                        visit(&block.graph, condition, &mut seen, &mut output);
                    }
                }
                Event::ReadMemory {
                    address, result, ..
                } => {
                    visit(&block.graph, address, &mut seen, &mut output);
                    seen[result] = true;
                }
                Event::WriteMemory {
                    address,
                    value,
                    result,
                    ..
                } => {
                    visit(&block.graph, address, &mut seen, &mut output);
                    visit(&block.graph, value, &mut seen, &mut output);
                    if let Some(result) = result {
                        seen[result] = true;
                    }
                }
                _ => {}
            }
            output
        })
        .collect()
}

/// Pure work needed to publish one selected checkpoint. Intermediate
/// checkpoints stay unscheduled until an API boundary makes them observable.
pub fn values(block: &Block, roots: &[ValueId]) -> Vec<ScalarId> {
    fn value(block: &Block, id: ValueId, seen: &mut [bool], scalars: &mut Vec<ScalarId>) {
        if std::mem::replace(&mut seen[id], true) {
            return;
        }
        match &block.values[id] {
            Recipe::Scalar { value, .. } => scalars.push(*value),
            Recipe::Struct(fields) => {
                for (_, id) in fields {
                    value(block, *id, seen, scalars);
                }
            }
            Recipe::Union(_, id) => value(block, *id, seen, scalars),
            Recipe::Vector(ids) | Recipe::Deferred { args: ids, .. } => {
                for id in ids {
                    value(block, *id, seen, scalars);
                }
            }
            Recipe::Constant(_) | Recipe::Input(_) => {}
        }
    }
    let mut seen_values = vec![false; block.values.len()];
    let mut roots_scalars = Vec::new();
    for id in roots {
        value(block, *id, &mut seen_values, &mut roots_scalars);
    }
    let mut seen_scalars = vec![false; block.graph.scalars.len()];
    let mut output = Vec::new();
    for id in roots_scalars {
        visit(&block.graph, id, &mut seen_scalars, &mut output);
    }
    output
}

#[cfg(test)]
mod tests {
    use super::super::{Binary, ScalarType};
    use super::*;

    #[test]
    fn effects_are_not_recomputed_and_dead_exit_values_are_not_scheduled() {
        let mut block = Block::default();
        let address = block.graph.input(0, ScalarType::Bits(64), None);
        let loaded = block.graph.effect(0, ScalarType::Bits(64));
        block.events.push(Event::ReadMemory {
            address,
            result: loaded,
            bytes: 8,
            exclusive: false,
            error: 0,
        });
        let zero = block.graph.constant(ScalarType::Bits(64), 0);
        let condition = block
            .graph
            .binary(ScalarType::Bool, Binary::NotEqual, loaded, zero)
            .unwrap();
        let dead = block.graph.input(1, ScalarType::Bits(64), None);
        block.events.push(Event::Guard {
            condition,
            expected: true,
            exit: 1,
        });
        let stages = schedule(&block);
        assert_eq!(stages[0], vec![address]);
        assert_eq!(stages[1], vec![zero]);
        assert!(
            !stages
                .iter()
                .flatten()
                .any(|id| *id == loaded || *id == dead)
        );
    }
}
