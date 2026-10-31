use vmlab_hv_tcg::{FlatMemory, Interpreter, Value};

#[test]
fn arbitrary_return_propagates_sails_exception_flag_to_the_caller() {
    let source = "val raise : () -> %i\nfn raise() { have_exception = true; arbitrary; }\nval catch : () -> %i\nfn catch() { x : %i; x = raise(); jump have_exception goto 5; return = x; goto 7; have_exception = false; return = 7; end; }";
    let document = vmlab_hv_sail_ir::parse(source).unwrap();
    let program = vmlab_hv_tcg_ir::lower(&document.definitions).unwrap();
    let mut interpreter = Interpreter::new(&program, FlatMemory::new(0, vec![]));
    interpreter.initialize().unwrap();
    assert_eq!(interpreter.call("catch", vec![]).unwrap(), Value::int(7));
}
