use vmlab_hv_tcg::{FlatMemory, Interpreter, Value};

#[test]
fn transitive_dependencies_invalidate_after_external_mutation() {
    let source = "register state : %i\nval leaf : () -> %i\nfn leaf() { return = state; end; }\nval outer : () -> %i\nfn outer() { return = leaf(); end; }";
    let document = vmlab_hv_sail_ir::parse(source).unwrap();
    let mut program = vmlab_hv_tcg_ir::lower(&document.definitions).unwrap();
    // Meet the runtime's minimum-size threshold without changing behavior.
    for function in program.functions.values_mut() {
        let padding = 40 - function.code.len();
        function.code.splice(
            0..0,
            (0..padding).map(|_| {
                vmlab_hv_tcg_ir::Op::Specialize("padding".into(), vmlab_hv_tcg_ir::Type::Unit)
            }),
        );
    }
    let effects = vmlab_hv_tcg_ir::effects(&program);
    assert!(effects["outer"].pure());
    assert!(effects["outer"].reads.contains("state"));
    let mut interpreter = Interpreter::new(&program, FlatMemory::new(0, vec![]));
    interpreter.initialize().unwrap();
    interpreter.globals.insert("state".into(), Value::int(7));
    assert_eq!(interpreter.call("outer", vec![]).unwrap(), Value::int(7));
    let start = interpreter.operations;
    assert_eq!(interpreter.call("outer", vec![]).unwrap(), Value::int(7));
    assert_eq!(interpreter.operations - start, 1);
    *interpreter.globals.get_mut("state").unwrap() = Value::int(9);
    assert_eq!(interpreter.call("outer", vec![]).unwrap(), Value::int(9));
    interpreter.globals.insert("state".into(), Value::int(11));
    assert_eq!(interpreter.call("outer", vec![]).unwrap(), Value::int(11));
}

#[test]
fn external_and_reference_effects_are_never_pure() {
    let source = "val output = \"print\" : (%string) -> %unit\nval observe : () -> %unit\nfn observe() { return = output(\"hello\"); end; }\nval unknown : () -> %unit\nval caller : () -> %unit\nfn caller() { return = unknown(); end; }";
    let document = vmlab_hv_sail_ir::parse(source).unwrap();
    let program = vmlab_hv_tcg_ir::lower(&document.definitions).unwrap();
    let effects = vmlab_hv_tcg_ir::effects(&program);
    assert!(!effects["observe"].pure());
    assert!(!effects["caller"].pure());
}

#[test]
fn a_state_changing_call_is_not_reused_for_its_new_input_state() {
    let source = "register state : %i\nval advance : () -> %i\nfn advance() { state = @iadd(state, 1); return = state; end; }";
    let document = vmlab_hv_sail_ir::parse(source).unwrap();
    let mut program = vmlab_hv_tcg_ir::lower(&document.definitions).unwrap();
    let function = program.functions.get_mut("advance").unwrap();
    function.code.splice(
        0..0,
        (0..40).map(|_| {
            vmlab_hv_tcg_ir::Op::Specialize("padding".into(), vmlab_hv_tcg_ir::Type::Unit)
        }),
    );
    let mut interpreter = Interpreter::new(&program, FlatMemory::new(0, vec![]));
    interpreter.initialize().unwrap();
    for value in 1..5 {
        assert_eq!(
            interpreter.call("advance", vec![]).unwrap(),
            Value::int(value)
        );
    }
}
