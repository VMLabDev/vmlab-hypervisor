#[cfg(feature = "sail")]
fn main() -> Result<(), Box<dyn std::error::Error>> {
    use std::path::PathBuf;
    use vmlab_hv_lockstep::{StepCpu, corpus::TestImage, sail::Sail};
    let library =
        PathBuf::from(std::env::var_os("VMLAB_SAIL_ORACLE").ok_or("set VMLAB_SAIL_ORACLE")?);
    let config =
        PathBuf::from(std::env::var_os("VMLAB_SAIL_CONFIG").ok_or("set VMLAB_SAIL_CONFIG")?);
    let mut count = 0;
    let mut pair = None;
    for path in std::env::args_os().skip(1) {
        let image = TestImage::elf(&std::fs::read(&path)?, 0x80000000, 16 * 1024 * 1024)?;
        if pair.is_none() {
            let mut a = Sail::open(&library, &config, image.memory.clone())?;
            let b = Sail::open(&library, &config, image.memory.clone())?;
            let initial = a.state()?;
            pair = Some((a, b, initial));
        }
        let (a, b, initial) = pair.as_mut().expect("initialized above");
        let steps = image.run(a, b, initial, 1_000_000)?;
        println!(
            "{}: matched {steps} instructions, tohost pass",
            PathBuf::from(path).display()
        );
        count += 1;
    }
    if count == 0 {
        return Err("supply at least one RV64 ELF path".into());
    }
    Ok(())
}

#[cfg(not(feature = "sail"))]
fn main() {
    eprintln!("the corpus driver requires --features sail");
    std::process::exit(2);
}
