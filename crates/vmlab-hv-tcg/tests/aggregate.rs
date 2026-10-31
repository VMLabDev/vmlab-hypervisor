use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::collections::BTreeMap;
use vmlab_hv_tcg::{FlatMemory, Interpreter, Value};
use vmlab_hv_tcg_ir::{Expr, Function, Location, Op, Program, Target, Type};

struct CountedAllocator;

thread_local! {
    static ALLOCATED: Cell<Option<usize>> = const { Cell::new(None) };
}

fn account(bytes: usize) {
    let _ = ALLOCATED.try_with(|total| {
        if let Some(previous) = total.get() {
            total.set(Some(previous.saturating_add(bytes)));
        }
    });
}

// SAFETY: allocation operations delegate to System with the original arguments;
// accounting uses allocation-free thread-local cells and cannot overflow.
unsafe impl GlobalAlloc for CountedAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        account(layout.size());
        // SAFETY: the caller supplies the layout required by GlobalAlloc.
        unsafe { System.alloc(layout) }
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        account(layout.size());
        // SAFETY: the caller supplies the layout required by GlobalAlloc.
        unsafe { System.alloc_zeroed(layout) }
    }

    unsafe fn dealloc(&self, pointer: *mut u8, layout: Layout) {
        // SAFETY: the allocation and layout came from this System allocator.
        unsafe { System.dealloc(pointer, layout) }
    }

    unsafe fn realloc(&self, pointer: *mut u8, layout: Layout, size: usize) -> *mut u8 {
        account(size);
        // SAFETY: the caller supplies a valid allocation, layout, and size.
        unsafe { System.realloc(pointer, layout, size) }
    }
}

#[global_allocator]
static ALLOCATOR: CountedAllocator = CountedAllocator;

fn program(operation: Op, result: Type) -> Program {
    let function = Function {
        name: "select".into(),
        args: vec![("index".into(), Type::Integer(None))],
        result,
        locals: BTreeMap::new(),
        code: vec![operation, Op::Return],
    };
    let mut program = Program::default();
    program.functions.insert(function.name.clone(), function);
    program
}

fn vector_call(name: &str) -> Op {
    Op::Call(
        Location::Id("return".into()),
        Target::Intrinsic(name.into()),
        vec![Expr::Id("state".into()), Expr::Id("index".into())],
    )
}

fn allocations(indexed: bool, entries: usize, vector: bool) -> usize {
    let operation = if vector {
        vector_call("vector_access")
    } else {
        Op::Assign(
            Location::Id("return".into()),
            Expr::Field(Box::new(Expr::Id("state".into())), "leaf".into()),
        )
    };
    let program = program(operation, Type::Integer(None));
    let mut interpreter = Interpreter::new(&program, FlatMemory::new(0, vec![]));
    interpreter.indexed = indexed;
    interpreter.memoize = false;
    let values = Value::Vector(vec![Value::int(7); entries]);
    let state = if vector {
        values
    } else {
        Value::Struct(BTreeMap::from([
            ("leaf".into(), Value::int(7)),
            ("unused".into(), values),
        ]))
    };
    interpreter.globals.insert("state".into(), state);
    assert_eq!(
        interpreter.call("select", vec![Value::int(0)]).unwrap(),
        Value::int(7)
    );
    ALLOCATED.with(|total| total.set(Some(0)));
    for _ in 0..10 {
        assert_eq!(
            interpreter.call("select", vec![Value::int(0)]).unwrap(),
            Value::int(7)
        );
    }
    ALLOCATED.with(|total| total.replace(None).unwrap())
}

#[test]
fn field_selection_does_not_clone_unread_fields() {
    for indexed in [false, true] {
        let small = allocations(indexed, 1, false);
        let large = allocations(indexed, 4096, false);
        eprintln!("field indexed={indexed}: {small} -> {large} allocated bytes");
        assert!(
            large <= small + 4096,
            "indexed={indexed}: {small} -> {large} allocated bytes"
        );
    }
}

#[test]
fn vector_selection_does_not_clone_unread_entries() {
    for indexed in [false, true] {
        let small = allocations(indexed, 1, true);
        let large = allocations(indexed, 4096, true);
        eprintln!("vector indexed={indexed}: {small} -> {large} allocated bytes");
        assert!(
            large <= small + 4096,
            "indexed={indexed}: {small} -> {large} allocated bytes"
        );
    }
}

#[test]
fn borrowed_bit_and_vector_access_preserve_values_and_errors() {
    for name in ["vector_access", "@bvaccess"] {
        let program = program(vector_call(name), Type::Bits(Some(1)));
        let mut observed = Vec::new();
        for indexed in [false, true] {
            let mut interpreter = Interpreter::new(&program, FlatMemory::new(0, vec![]));
            interpreter.indexed = indexed;
            interpreter.memoize = false;
            let mut results = Vec::new();
            for state in [
                Value::bits(8, 0x80u8),
                Value::Vector(vec![Value::bits(1, 0u8), Value::bits(1, 1u8)]),
                Value::Bool(false),
            ] {
                interpreter.globals.insert("state".into(), state);
                for index in [-1, 0, 1, 7, 8] {
                    results.push(
                        interpreter
                            .call("select", vec![Value::int(index)])
                            .map_err(|error| error.to_string()),
                    );
                }
            }
            assert_eq!(results[1], Ok(Value::bits(1, 0u8)));
            assert_eq!(results[3], Ok(Value::bits(1, 1u8)));
            assert_eq!(results[6], Ok(Value::bits(1, 0u8)));
            assert_eq!(results[7], Ok(Value::bits(1, 1u8)));
            for index in [0, 4, 5, 8, 9, 10, 11, 12, 13, 14] {
                assert!(results[index].is_err());
            }
            observed.push(results);
        }
        assert_eq!(observed[0], observed[1]);
    }
}

#[test]
fn nested_field_and_union_projection_returns_an_owned_value() {
    let expression = Expr::Field(
        Box::new(Expr::As(
            Box::new(Expr::Field(
                Box::new(Expr::Id("state".into())),
                "choice".into(),
            )),
            "Some".into(),
        )),
        "items".into(),
    );
    let program = program(
        Op::Assign(Location::Id("return".into()), expression),
        Type::Vector(Box::new(Type::Integer(None))),
    );
    let state = Value::Struct(BTreeMap::from([(
        "choice".into(),
        Value::Union(
            "Some".into(),
            Box::new(Value::Struct(BTreeMap::from([(
                "items".into(),
                Value::Vector(vec![Value::int(7)]),
            )]))),
        ),
    )]));
    for indexed in [false, true] {
        let mut interpreter = Interpreter::new(&program, FlatMemory::new(0, vec![]));
        interpreter.indexed = indexed;
        interpreter.memoize = false;
        interpreter.globals.insert("state".into(), state.clone());
        let Value::Vector(mut selected) = interpreter.call("select", vec![Value::int(0)]).unwrap()
        else {
            panic!("expected selected vector");
        };
        selected[0] = Value::int(9);
        assert_eq!(interpreter.globals["state"], state);
    }
}
