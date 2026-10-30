use crate::Value;
use std::collections::{BTreeMap, BTreeSet};
use vmlab_hv_tcg_ir::{Expr, Location, Op, Program, Target};

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct CacheKey {
    pub address: u64,
    pub generation: u64,
    pub compressed: bool,
    pub instruction: u32,
    pub context: Vec<Option<Value>>,
}

pub(crate) struct Dependencies {
    symbols: Vec<String>,
    pub cacheable: bool,
}

impl Dependencies {
    /// Derive decoder inputs from the generated call graph, including globals
    /// written on exception paths. A model upgrade must not need a manual list.
    pub fn new(program: &Program) -> Self {
        let mut globals: BTreeSet<_> = program.globals.keys().cloned().collect();
        globals
            .extend(["have_exception", "current_exception", "throw_location"].map(str::to_owned));
        let mut dependencies = Self {
            symbols: Vec::new(),
            cacheable: true,
        };
        let mut symbols = BTreeSet::new();
        let mut visited = BTreeSet::new();
        let mut todo = vec![
            "zext_decode".to_owned(),
            "zext_decode_compressed".to_owned(),
        ];
        while let Some(name) = todo.pop() {
            if !visited.insert(name.clone()) {
                continue;
            }
            let Some(function) = program.functions.get(&name) else {
                dependencies.cacheable = false;
                continue;
            };
            for op in &function.code {
                match op {
                    Op::Declare(_, _, Some(expr)) | Op::Assign(_, expr) | Op::Branch(expr, _) => {
                        reads(expr, &globals, &mut symbols)
                    }
                    Op::Call(_, target, args) => {
                        for arg in args {
                            reads(arg, &globals, &mut symbols);
                        }
                        match target {
                            Target::Function(name) => todo.push(name.clone()),
                            Target::Memory { .. } => dependencies.cacheable = false,
                            Target::Intrinsic(name)
                                if matches!(
                                    name.as_str(),
                                    "print"
                                        | "print_endline"
                                        | "print_int"
                                        | "print_bits"
                                        | "print_string"
                                        | "reset_registers"
                                        | "load_reservation"
                                        | "match_reservation"
                                        | "cancel_reservation"
                                        | "valid_reservation"
                                        | "event_callback"
                                ) =>
                            {
                                dependencies.cacheable = false
                            }
                            _ => {}
                        }
                    }
                    _ => {}
                }
                if let Op::Assign(location, _) | Op::Call(location, _, _) = op {
                    let name = root(location);
                    if globals.contains(name) {
                        symbols.insert(name.to_owned());
                    }
                }
            }
        }
        dependencies.symbols = symbols.into_iter().collect();
        dependencies
    }

    pub fn snapshot(&self, globals: &BTreeMap<String, Value>) -> Vec<Option<Value>> {
        self.symbols
            .iter()
            .map(|name| globals.get(name).cloned())
            .collect()
    }
}

fn root(location: &Location) -> &str {
    match location {
        Location::Id(name) => name,
        Location::Field(base, _) | Location::Deref(base) => root(base),
    }
}

fn reads(expr: &Expr, globals: &BTreeSet<String>, symbols: &mut BTreeSet<String>) {
    match expr {
        Expr::Id(name) | Expr::Ref(name) if globals.contains(name) => {
            symbols.insert(name.clone());
        }
        Expr::Struct(_, fields) => {
            for (_, expr) in fields {
                reads(expr, globals, symbols);
            }
        }
        Expr::Field(expr, _) | Expr::Is(expr, _) | Expr::As(expr, _) => {
            reads(expr, globals, symbols)
        }
        Expr::Op(_, _, args) => {
            for expr in args {
                reads(expr, globals, symbols);
            }
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn pinned_decoder_cache_tracks_architectural_inputs() {
        let dependencies = Dependencies::new(vmlab_hv_tcg_riscv64::model());
        assert!(dependencies.cacheable);
        for name in ["zmstatus", "zcur_privilege", "zmisa", "zvtype"] {
            assert!(dependencies.symbols.iter().any(|symbol| symbol == name), "missing {name}");
        }
    }
}
