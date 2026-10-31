use crate::{Expr, Function, Location, Op};
use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

pub struct Liveness {
    pub names: Arc<Vec<String>>,
    pub slots: BTreeMap<String, usize>,
    pub live: Vec<Vec<u64>>,
}

fn root(location: &Location) -> Option<&str> {
    match location {
        Location::Id(name) => Some(name),
        Location::Field(base, _) => root(base),
        Location::Deref(_) => None,
    }
}

fn expression(expr: &Expr, names: &mut BTreeSet<String>) {
    match expr {
        Expr::Id(name) | Expr::Ref(name) => {
            names.insert(name.clone());
        }
        Expr::Struct(_, fields) => {
            for (_, expr) in fields {
                expression(expr, names);
            }
        }
        Expr::Field(expr, _) | Expr::As(expr, _) | Expr::Is(expr, _) => expression(expr, names),
        Expr::Op(_, _, args) => {
            for arg in args {
                expression(arg, names);
            }
        }
        _ => {}
    }
}

pub fn analyze(function: &Function) -> Liveness {
    let mut names: BTreeSet<String> = function.args.iter().map(|(name, _)| name.clone()).collect();
    names.insert("return".into());
    for op in &function.code {
        match op {
            Op::Declare(name, _, _) => {
                names.insert(name.clone());
            }
            Op::Assign(location, _) | Op::Call(location, _, _) => {
                if let Some(name) = root(location) {
                    names.insert(name.into());
                }
            }
            _ => {}
        }
    }
    let names: Vec<_> = names.into_iter().collect();
    let slots: BTreeMap<_, _> = names
        .iter()
        .cloned()
        .enumerate()
        .map(|(i, name)| (name, i))
        .collect();
    let words = names.len().div_ceil(64);
    let bits = |set: BTreeSet<String>| {
        let mut result = vec![0u64; words];
        for name in set {
            if let Some(slot) = slots.get(&name) {
                result[slot / 64] |= 1 << (slot % 64);
            }
        }
        result
    };
    let transfers: Vec<_> = function
        .code
        .iter()
        .enumerate()
        .map(|(pc, op)| {
            let mut uses = BTreeSet::new();
            let mut kills = BTreeSet::new();
            let mut successors = vec![pc + 1];
            match op {
                Op::Declare(name, _, init) => {
                    kills.insert(name.clone());
                    if let Some(expr) = init {
                        expression(expr, &mut uses);
                    }
                }
                Op::Assign(_, expr) => expression(expr, &mut uses),
                Op::Call(_, _, args) => {
                    for arg in args {
                        expression(arg, &mut uses);
                    }
                }
                Op::Branch(expr, target) => {
                    expression(expr, &mut uses);
                    successors.push(*target);
                }
                Op::Jump(target) => successors = vec![*target],
                Op::Return | Op::Arbitrary => {
                    uses.insert("return".into());
                    successors.clear();
                }
                Op::Raise(_) => successors.clear(),
                _ => {}
            }
            if let Op::Assign(location, _) | Op::Call(location, _, _) = op {
                match location {
                    Location::Id(name) => {
                        kills.insert(name.clone());
                    }
                    Location::Field(_, _) => {
                        if let Some(name) = root(location) {
                            uses.insert(name.into());
                        }
                    }
                    // A reference can name any current local. Preserve all of them.
                    Location::Deref(_) => uses.extend(names.iter().cloned()),
                }
            }
            (bits(uses), bits(kills), successors)
        })
        .collect();
    let mut live = vec![vec![0u64; words]; function.code.len()];
    loop {
        let mut changed = false;
        for (pc, (uses, kills, successors)) in transfers.iter().enumerate().rev() {
            for word in 0..words {
                let outgoing = successors
                    .iter()
                    .filter_map(|next| live.get(*next))
                    .fold(0, |value, next| value | next[word]);
                let incoming = uses[word] | (outgoing & !kills[word]);
                changed |= incoming != live[pc][word];
                live[pc][word] = incoming;
            }
        }
        if !changed {
            break;
        }
    }
    Liveness {
        names: Arc::new(names),
        slots,
        live,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Type;

    #[test]
    fn loops_and_partial_struct_updates_keep_their_inputs() {
        let function = Function {
            name: "live".into(),
            args: vec![
                ("counter".into(), Type::Integer(None)),
                ("packet".into(), Type::Unit),
            ],
            result: Type::Integer(None),
            locals: BTreeMap::new(),
            code: vec![
                Op::Declare("dead".into(), Type::Integer(None), None),
                Op::Assign(
                    Location::Field(Box::new(Location::Id("packet".into())), "field".into()),
                    Expr::Id("counter".into()),
                ),
                Op::Branch(Expr::Id("counter".into()), 1),
                Op::Assign(Location::Id("return".into()), Expr::Id("packet".into())),
                Op::Return,
            ],
        };
        let live = analyze(&function);
        let contains = |pc: usize, name: &str| {
            let slot = live.slots[name];
            live.live[pc][slot / 64] & (1 << (slot % 64)) != 0
        };
        assert!(!contains(0, "dead"));
        assert!(contains(1, "packet"));
        assert!(contains(1, "counter"));
        assert!(!contains(4, "counter"));
        assert!(contains(4, "return"));
    }
}
