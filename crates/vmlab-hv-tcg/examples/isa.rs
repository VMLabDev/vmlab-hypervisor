use vmlab_hv_tcg::{FlatMemory, Riscv64, Value};
fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut cpu = Riscv64::new(FlatMemory::new(0x8000_0000, vec![0; 4096]))?;
    // Enable floating-point and vector state before querying the model's list.
    let mstatus = cpu.read_csr(0x300)?;
    cpu.write_csr(0x300, mstatus | (3 << 13) | (3 << 9))?;
    let variants = cpu
        .interpreter
        .program
        .enums
        .values()
        .find(|members| members.iter().any(|s| s == "zExt_M"))
        .ok_or("extension enum")?
        .clone();
    let mut enabled = vec!["I".to_owned()];
    for variant in variants {
        if cpu
            .interpreter
            .call("zcurrentlyEnabled", vec![Value::Enum(variant.clone())])?
            .boolean()?
        {
            enabled.push(variant.trim_start_matches("zExt_").replace("zz", "z"));
        }
    }
    println!("{}", enabled.join(" "));
    Ok(())
}
