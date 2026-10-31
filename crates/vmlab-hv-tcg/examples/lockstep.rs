#[cfg(feature = "sail")]
fn main() -> Result<(), Box<dyn std::error::Error>> {
    use std::path::PathBuf;
    use vmlab_hv_lockstep::{StepCpu, corpus::TestImage, sail::Sail};
    use vmlab_hv_tcg::lockstep::LockstepCpu;
    let library =
        PathBuf::from(std::env::var_os("VMLAB_SAIL_ORACLE").ok_or("set VMLAB_SAIL_ORACLE")?);
    let config =
        PathBuf::from(std::env::var_os("VMLAB_SAIL_CONFIG").ok_or("set VMLAB_SAIL_CONFIG")?);
    let mut pair = None;
    let mut count = 0;
    let mut failures = 0;
    for path in std::env::args_os().skip(1) {
        let image = TestImage::elf(&std::fs::read(&path)?, 0x80000000, 16 * 1024 * 1024)?;
        if pair.is_none() {
            let mut sail = Sail::open(&library, &config, image.memory.clone())?;
            let initial = sail.state()?;
            pair = Some((
                LockstepCpu::for_lockstep(image.memory.clone())?,
                sail,
                initial,
            ));
        }
        let (interpreter, sail, initial) = pair.as_mut().ok_or("missing CPU pair")?;
        match image.run(interpreter, sail, initial, 1_000_000) {
            Ok(steps) => println!(
                "{}: matched {steps} instructions, tohost pass",
                PathBuf::from(&path).display()
            ),
            Err(error) => {
                println!("{}: {error}", PathBuf::from(&path).display());
                failures += 1;
            }
        }
        count += 1;
    }
    if count == 0 {
        return Err("supply RV64 ELF paths".into());
    }
    println!(
        "{} passed, {failures} failed, {count} total",
        count - failures
    );
    if failures != 0 {
        return Err("lockstep divergences found".into());
    }
    Ok(())
}

#[cfg(not(feature = "sail"))]
fn main() {
    eprintln!("the lockstep driver requires --features sail");
    std::process::exit(2);
}
