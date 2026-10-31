use crate::{Expr, Location, Op, Program, Target};
use std::collections::{BTreeMap, BTreeSet};

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Effects {
    pub reads: BTreeSet<String>,
    pub writes: BTreeSet<String>,
    pub external: bool,
}

impl Effects {
    pub fn pure(&self) -> bool {
        !self.external && self.writes.is_empty()
    }
}

/// Conservative, transitive effects for specialization and memoization. Unknown
/// calls, memory, references, and platform operations always remain observable.
pub fn effects(program: &Program) -> BTreeMap<String, Effects> {
    let mut globals: BTreeSet<_> = program.globals.keys().cloned().collect();
    globals.extend(["have_exception", "current_exception", "throw_location"].map(str::to_owned));
    globals.extend(program.enums.values().flatten().cloned());
    let mut result = BTreeMap::new();
    let mut calls = BTreeMap::new();
    for (name, function) in &program.functions {
        let mut effect = Effects::default();
        let mut callees = BTreeSet::new();
        for op in &function.code {
            match op {
                Op::Declare(_, _, Some(expr)) | Op::Assign(_, expr) | Op::Branch(expr, _) => {
                    expression(expr, &globals, &mut effect);
                }
                Op::Call(_, target, args) => {
                    for arg in args {
                        expression(arg, &globals, &mut effect);
                    }
                    match target {
                        Target::Function(name) => {
                            callees.insert(name.clone());
                        }
                        Target::Memory { .. } => effect.external = true,
                        Target::Intrinsic(name) => effect.external |= !pure_intrinsic(name),
                        _ => {}
                    }
                }
                _ => {}
            }
            if let Op::Assign(location, _) | Op::Call(location, _, _) = op {
                destination(location, &globals, &mut effect);
            }
        }
        result.insert(name.clone(), effect);
        calls.insert(name.clone(), callees);
    }
    loop {
        let mut changed = false;
        for (name, callees) in &calls {
            let mut effect = result[name].clone();
            for callee in callees {
                if let Some(other) = result.get(callee) {
                    effect.reads.extend(other.reads.iter().cloned());
                    effect.writes.extend(other.writes.iter().cloned());
                    effect.external |= other.external;
                } else {
                    effect.external = true;
                }
            }
            if effect != result[name] {
                result.insert(name.clone(), effect);
                changed = true;
            }
        }
        if !changed {
            return result;
        }
    }
}

fn destination(location: &Location, globals: &BTreeSet<String>, effect: &mut Effects) {
    match location {
        Location::Id(name) if globals.contains(name) => {
            effect.writes.insert(name.clone());
        }
        Location::Field(base, _) => destination(base, globals, effect),
        Location::Deref(_) => effect.external = true,
        _ => {}
    }
}

fn expression(expr: &Expr, globals: &BTreeSet<String>, effect: &mut Effects) {
    match expr {
        Expr::Id(name) if globals.contains(name) => {
            effect.reads.insert(name.clone());
        }
        Expr::Ref(_) => effect.external = true,
        Expr::Struct(_, fields) => {
            for (_, expr) in fields {
                expression(expr, globals, effect);
            }
        }
        Expr::Field(expr, _) | Expr::As(expr, _) | Expr::Is(expr, _) => {
            expression(expr, globals, effect);
        }
        Expr::Op(name, _, args) => {
            effect.external |= !pure_intrinsic(name);
            for arg in args {
                expression(arg, globals, effect);
            }
        }
        _ => {}
    }
}

pub fn pure_intrinsic(name: &str) -> bool {
    matches!(
        name,
        "%i->%i64"
            | "%i64->%i"
            | "monomorphize"
            | "@not"
            | "not"
            | "@and"
            | "and_bool"
            | "@or"
            | "or_bool"
            | "@eq"
            | "eq_int"
            | "eq_bool"
            | "eq_bits"
            | "eq_string"
            | "eq_anything"
            | "@neq"
            | "neq_bits"
            | "@lt"
            | "lt"
            | "@lteq"
            | "lteq"
            | "@gt"
            | "gt"
            | "@gteq"
            | "gteq"
            | "@iadd"
            | "add_int"
            | "@isub"
            | "sub_int"
            | "mult_int"
            | "neg_int"
            | "abs_int"
            | "min_int"
            | "max_int"
            | "tdiv_int"
            | "tmod_int"
            | "ediv_int"
            | "pow2"
            | "zeros"
            | "undefined_bitvector"
            | "undefined_bool"
            | "undefined_vector"
            | "@unsigned"
            | "sail_unsigned"
            | "@signed"
            | "sail_signed"
            | "@bvnot"
            | "not_bits"
            | "@bvor"
            | "or_bits"
            | "@bvand"
            | "and_bits"
            | "@bvxor"
            | "xor_bits"
            | "@bvadd"
            | "add_bits"
            | "add_bits_int"
            | "@bvsub"
            | "sub_bits"
            | "sub_bits_int"
            | "append"
            | "append_64"
            | "vector_init"
            | "zinternal_vector_init"
            | "zinternal_vector_update"
            | "@bvaccess"
            | "@concat"
            | "length"
            | "length_bits"
            | "length_list"
            | "vector_access"
            | "vector_update"
            | "vector_subrange"
            | "vector_update_subrange"
            | "slice"
            | "slice_bits"
            | "update_subrange_bits"
            | "@slice"
            | "@sslice"
            | "get_slice_int"
            | "sign_extend"
            | "zero_extend"
            | "sail_truncate"
            | "@zero_extend"
            | "shiftl"
            | "shift_bits_left"
            | "shiftr"
            | "shift_bits_right"
            | "arith_shiftr"
            | "count_leading_zeros"
            | "count_trailing_zeros"
            | "@hd"
            | "@tl"
            | "@is_empty"
            | "cons"
            | "zsail_assert"
            | "zupdate_fbits"
            | "concat_str"
            | "dec_str"
            | "hex_str"
            | "string_of_bits"
            | "sys_enable_experimental_extensions"
            | "parse_hex_bits"
            | "valid_hex_bits"
    )
}
