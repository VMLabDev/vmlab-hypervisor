use vmlab_hv_tcg::{FlatMemory, Interpreter, Value};

#[test]
fn indexed_and_named_execution_agree_on_branches_globals_and_exceptions() {
    let source = "register state : %i\nval raise : () -> %i\nfn raise() { have_exception = true; arbitrary; }\nval catch : () -> %i\nfn catch() { x : %i; x = raise(); jump have_exception goto 5; return = x; goto 7; have_exception = false; return = state; end; }";
    let document = vmlab_hv_sail_ir::parse(source).unwrap();
    let program = vmlab_hv_tcg_ir::lower(&document.definitions).unwrap();
    let mut observed = Vec::new();
    for indexed in [false, true] {
        let mut cpu = Interpreter::new(&program, FlatMemory::new(0, vec![]));
        cpu.initialize().unwrap();
        cpu.memoize = false;
        cpu.indexed = indexed;
        cpu.globals.insert("state".into(), Value::int(19));
        assert_eq!(cpu.call("catch", vec![]).unwrap(), Value::int(19));
        observed.push(((*cpu.globals).clone(), cpu.operations));
    }
    assert_eq!(observed[0], observed[1]);
}
