use vmlab_hv_tcg::{FlatMemory, Memory, Riscv64};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<_> = std::env::args().skip(1).collect();
    let mut memory = FlatMemory::new(0x8000_0000, vec![0; 16 * 1024 * 1024]);
    let (limit, iterations, tohost) = match args.first().map(String::as_str) {
        Some("loop") if args.len() == 2 => {
            let iterations: u64 = args[1].parse()?;
            if iterations == 0 || iterations > u64::MAX / 3 {
                return Err("invalid loop count".into());
            }
            for (i, instruction) in [0x00108093u32, 0xfff10113, 0xfe011ce3].iter().enumerate() {
                memory.write(0x8000_0000 + i as u64 * 4, &instruction.to_le_bytes())?;
            }
            (iterations * 3, Some(iterations), None)
        }
        Some("image") if args.len() == 4 => {
            memory.write(0x8000_0000, &std::fs::read(&args[1])?)?;
            (
                args[3].parse()?,
                None,
                Some(u64::from_str_radix(args[2].trim_start_matches("0x"), 16)?),
            )
        }
        _ => return Err("usage: micro loop ITERATIONS | image IMAGE TOHOST_HEX MAX_STEPS".into()),
    };
    let init = std::time::Instant::now();
    let mut cpu = Riscv64::new(memory)?;
    let initialization_seconds = init.elapsed().as_secs_f64();
    cpu.set_pc(0x8000_0000);
    if let Some(iterations) = iterations {
        cpu.set_gpr(2, iterations)?;
    }
    cpu.interpreter.operation_limit = u64::MAX;
    cpu.interpreter.memoize = std::env::var_os("VMLAB_TCG_NO_MEMO").is_none();
    cpu.interpreter.indexed = std::env::var_os("VMLAB_TCG_NO_INDEXED").is_none();
    if std::env::var_os("VMLAB_TCG_PROFILE").is_some() {
        cpu.interpreter.profile = Some(Default::default());
    }
    let operations = cpu.interpreter.operations;
    let start = std::time::Instant::now();
    let mut passed = false;
    for _ in 0..limit {
        cpu.step()?;
        if let Some(address) = tohost {
            let mut bytes = [0; 8];
            cpu.interpreter.memory.read(address, &mut bytes)?;
            match u64::from_le_bytes(bytes) {
                0 => {}
                1 => {
                    passed = true;
                    break;
                }
                value => return Err(format!("guest failed: tohost={value:#x}").into()),
            }
        }
    }
    let seconds = start.elapsed().as_secs_f64();
    if let Some(iterations) = iterations {
        if cpu.gpr(1)? != iterations || cpu.gpr(2)? != 0 || cpu.pc()? != 0x8000_000c {
            return Err("loop result mismatch".into());
        }
    } else if !passed {
        return Err(format!("instruction limit at PC {:#x}", cpu.pc()?).into());
    }
    let profile = cpu.interpreter.profile.map(|profile| {
        let mut functions: Vec<_> = profile.functions.into_iter().collect();
        functions.sort_by_key(|(_, (_, own, _))| std::cmp::Reverse(*own));
        functions.truncate(30);
        functions
    });
    println!(
        "{}",
        serde_json::json!({
            "instructions": cpu.steps, "seconds": seconds,
            "mips": cpu.steps as f64 / seconds / 1e6,
            "initialization_seconds": initialization_seconds,
            "ir_operations": cpu.interpreter.operations - operations,
            "functions": profile,
        })
    );
    Ok(())
}
