use vmlab_hv_tcg::{FlatMemory, Interpreter, Memory, Value};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let program = vmlab_hv_tcg_riscv64::model();
    let mut interpreter = Interpreter::new(
        program,
        FlatMemory::new(0x8000_0000, vec![0; 16 * 1024 * 1024]),
    );
    interpreter.initialize()?;
    interpreter.call("zinitializze_registers", vec![Value::Unit])?;
    interpreter.call("zreset", vec![Value::Unit])?;
    let instruction = interpreter.call("zext_decode", vec![Value::bits(32, 0x00500093u32)])?;
    println!("decoded: {instruction:?}");
    interpreter.call("zexecute", vec![instruction])?;
    println!(
        "x1: {:?}, {} IR operations",
        interpreter.globals["zx1"], interpreter.operations
    );
    interpreter
        .memory
        .write(0x8000_0000, &0x00500093u32.to_le_bytes())?;
    interpreter
        .globals
        .insert("zPC".into(), Value::bits(64, 0x8000_0000u64));
    interpreter
        .globals
        .insert("znextPC".into(), Value::bits(64, 0x8000_0000u64));
    let result = interpreter.call("ztry_step", vec![Value::int(0), Value::Bool(true)])?;
    println!("step: {result:?}, pc: {:?}", interpreter.globals["zPC"]);
    Ok(())
}
