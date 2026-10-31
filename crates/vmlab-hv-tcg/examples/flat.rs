use vmlab_hv_tcg::{FlatMemory, Riscv64};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<_> = std::env::args().skip(1).collect();
    if args.is_empty() {
        return Err("usage: flat IMAGE [TOHOST_HEX] [MAX_STEPS]".into());
    }
    let mut image = std::fs::read(&args[0])?;
    image.resize(16 * 1024 * 1024, 0);
    let tohost = u64::from_str_radix(
        args.get(1)
            .map_or("80001000", String::as_str)
            .trim_start_matches("0x"),
        16,
    )?;
    let limit = args.get(2).map_or(Ok(100_000u64), |s| s.parse())?;
    let mut cpu = Riscv64::new(FlatMemory::new(0x8000_0000, image))?;
    cpu.set_pc(0x8000_0000);
    cpu.interpreter.operation_limit = 5_000_000_000;
    cpu.specialize = std::env::var_os("VMLAB_TCG_SPECIALIZE").is_some();
    let start = std::time::Instant::now();
    for _ in 0..limit {
        cpu.step()
            .map_err(|e| format!("pc {:#x}, step {}: {e}", cpu.pc().unwrap_or(0), cpu.steps))?;
        let offset = usize::try_from(tohost.checked_sub(0x8000_0000).ok_or("tohost below RAM")?)?;
        let bytes = cpu
            .interpreter
            .memory
            .bytes()
            .get(offset..offset + 8)
            .ok_or("tohost outside RAM")?;
        let value = u64::from_le_bytes(bytes.try_into()?);
        if value != 0 {
            println!(
                "tohost={value:#x} steps={} seconds={:.6} MIPS={:.6}",
                cpu.steps,
                start.elapsed().as_secs_f64(),
                cpu.steps as f64 / start.elapsed().as_secs_f64() / 1e6
            );
            if value != 1 {
                return Err(format!("guest test failed: {}", value >> 1).into());
            }
            return Ok(());
        }
    }
    Err(format!("step limit: pc {:#x}, steps {}", cpu.pc()?, cpu.steps).into())
}
