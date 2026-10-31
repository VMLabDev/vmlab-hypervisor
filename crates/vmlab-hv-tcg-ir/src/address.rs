use crate::{Expr, Function, Location, Op, Program, Target};
use std::collections::{BTreeMap, BTreeSet};

type Flow = BTreeSet<usize>;

#[derive(Clone, Default, PartialEq, Eq)]
struct Summary {
    addresses: Flow,
    returned: Flow,
}

fn expression(expr: &Expr, locals: &BTreeMap<String, Flow>) -> Flow {
    match expr {
        Expr::Id(name) | Expr::Ref(name) => locals.get(name).cloned().unwrap_or_default(),
        Expr::Field(value, _) | Expr::As(value, _) | Expr::Is(value, _) => {
            expression(value, locals)
        }
        Expr::Struct(_, fields) => fields
            .iter()
            .flat_map(|(_, value)| expression(value, locals))
            .collect(),
        Expr::Op(_, _, args) => args
            .iter()
            .flat_map(|arg| expression(arg, locals))
            .collect(),
        _ => Flow::new(),
    }
}

fn destination(location: &Location) -> &str {
    match location {
        Location::Id(name) => name,
        Location::Field(base, _) | Location::Deref(base) => destination(base),
    }
}

fn analyze(function: &Function, summaries: &BTreeMap<String, Summary>) -> Summary {
    let mut locals: BTreeMap<_, _> = function
        .args
        .iter()
        .enumerate()
        .map(|(id, (name, _))| (name.clone(), Flow::from([id])))
        .collect();
    let mut summary = Summary::default();
    loop {
        let before = locals.values().map(Flow::len).sum::<usize>();
        for op in &function.code {
            let (name, flow) = match op {
                Op::Declare(name, _, Some(value)) => (name.as_str(), expression(value, &locals)),
                Op::Assign(location, value) => (destination(location), expression(value, &locals)),
                Op::Call(location, target, args) => {
                    let args: Vec<_> = args.iter().map(|arg| expression(arg, &locals)).collect();
                    let selected = |indices: &Flow| {
                        indices
                            .iter()
                            .filter_map(|id| args.get(*id))
                            .flat_map(|flow| flow.iter().copied())
                            .collect::<Flow>()
                    };
                    let flow = match target {
                        Target::Memory { .. } => {
                            summary
                                .addresses
                                .extend(args.get(2).into_iter().flatten().copied());
                            // Read data does not inherit its address's numeric provenance.
                            Flow::new()
                        }
                        Target::Function(callee) if summaries.contains_key(callee) => {
                            let callee = &summaries[callee];
                            summary.addresses.extend(selected(&callee.addresses));
                            selected(&callee.returned)
                        }
                        _ => args.into_iter().flatten().collect(),
                    };
                    (destination(location), flow)
                }
                _ => continue,
            };
            locals.entry(name.to_owned()).or_default().extend(flow);
        }
        if before == locals.values().map(Flow::len).sum::<usize>() {
            summary.returned = locals.get("return").cloned().unwrap_or_default();
            return summary;
        }
    }
}

/// Candidate address parameters through assignments, return values, and calls.
/// This conservative data flow selects speculation sites; it proves neither
/// that a value is an address nor that an access is permitted. Runtime numeric
/// guards and the generated memory checks remain required.
pub fn address_arguments(program: &Program) -> BTreeMap<String, BTreeSet<usize>> {
    let mut summaries: BTreeMap<_, _> = program
        .functions
        .keys()
        .map(|name| (name.clone(), Summary::default()))
        .collect();
    loop {
        let mut changed = false;
        for (name, function) in &program.functions {
            let summary = analyze(function, &summaries);
            if summaries[name] != summary {
                summaries.insert(name.clone(), summary);
                changed = true;
            }
        }
        if !changed {
            return summaries
                .into_iter()
                .map(|(name, summary)| (name, summary.addresses))
                .collect();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Type;

    fn function(name: &str, names: &[&str], code: Vec<Op>) -> Function {
        Function {
            name: name.into(),
            args: names
                .iter()
                .map(|name| (name.to_string(), Type::Bits(Some(64))))
                .collect(),
            result: Type::Bits(Some(64)),
            locals: BTreeMap::new(),
            code,
        }
    }

    #[test]
    fn address_flow_crosses_helpers_but_not_memory_read_results() {
        let functions = [
            function(
                "read",
                &["address"],
                vec![
                    Op::Call(
                        Location::Id("return".into()),
                        Target::Memory {
                            name: "read_mem".into(),
                            write: false,
                            exclusive: false,
                            fetch: false,
                        },
                        vec![
                            Expr::Unit,
                            Expr::Unit,
                            Expr::Id("address".into()),
                            Expr::Integer("8".into(), None),
                        ],
                    ),
                    Op::Return,
                ],
            ),
            function(
                "identity",
                &["value"],
                vec![
                    Op::Assign(Location::Id("return".into()), Expr::Id("value".into())),
                    Op::Return,
                ],
            ),
            function(
                "chase",
                &["pointer", "data"],
                vec![
                    Op::Call(
                        Location::Id("address".into()),
                        Target::Function("identity".into()),
                        vec![Expr::Id("pointer".into())],
                    ),
                    Op::Call(
                        Location::Id("loaded".into()),
                        Target::Function("read".into()),
                        vec![Expr::Id("address".into())],
                    ),
                    Op::Call(
                        Location::Id("return".into()),
                        Target::Function("read".into()),
                        vec![Expr::Id("loaded".into())],
                    ),
                    Op::Return,
                ],
            ),
        ]
        .into_iter()
        .map(|function| (function.name.clone(), function))
        .collect();
        let program = Program {
            functions,
            ..Program::default()
        };
        let arguments = address_arguments(&program);
        assert_eq!(arguments["read"], Flow::from([0]));
        assert_eq!(arguments["chase"], Flow::from([0]));
        assert!(arguments["identity"].is_empty());
    }
}
