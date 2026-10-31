use vmlab_hv_tcg::{FlatMemory, Memory, Riscv64};

fn execute(
    cpu: &mut Riscv64<FlatMemory>,
    limit: u64,
    tohost: Option<u64>,
) -> Result<(), Box<dyn std::error::Error>> {
    let start = cpu.steps;
    let progress = std::env::var_os("VMLAB_TCG_PROGRESS").is_some();
    while cpu.steps - start < limit {
        let remaining = limit - (cpu.steps - start);
        let remaining = if progress {
            remaining.min(10_000)
        } else {
            remaining
        };
        if let Some(address) = tohost {
            cpu.run_until_store(remaining, address)?;
        } else {
            cpu.run(remaining)?;
        }
        if progress {
            eprintln!(
                "steps={} pc={:#x} recordings={} training={} native={} exits={} evictions={} max_trace={:?}",
                cpu.steps - start,
                cpu.pc()?,
                cpu.specialization_stats.recordings,
                cpu.specialization_stats.training_steps,
                cpu.specialization_stats.hits,
                cpu.specialization_stats.exits,
                cpu.specialization_stats.evictions,
                cpu.specialization_stats.max_trace
            );
        }
        if let Some(address) = tohost {
            let mut bytes = [0; 8];
            cpu.interpreter.memory.read(address, &mut bytes)?;
            match u64::from_le_bytes(bytes) {
                0 => {}
                1 => return Ok(()),
                value => return Err(format!("guest failed: tohost={value:#x}").into()),
            }
        }
    }
    if tohost.is_some() {
        return Err(format!("instruction limit at PC {:#x}", cpu.pc()?).into());
    }
    Ok(())
}

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
    cpu.specialize = std::env::var_os("VMLAB_TCG_SPECIALIZE").is_some();
    if std::env::var_os("VMLAB_TCG_PROFILE").is_some() {
        cpu.interpreter.profile = Some(Default::default());
    }
    let mut warmup_seconds = 0.0;
    if std::env::var_os("VMLAB_TCG_WARM").is_some() {
        let warm = std::time::Instant::now();
        let initial_globals = (*cpu.interpreter.globals).clone();
        let initial_ram = tohost.map(|_| cpu.interpreter.memory.bytes().to_vec());
        if iterations.is_some() {
            cpu.set_gpr(1, 1_000_000)?;
            cpu.set_gpr(2, 1_000_000)?;
        }
        for _ in 0..if iterations.is_some() { 1 } else { 2 } {
            execute(
                &mut cpu,
                if iterations.is_some() { 65_536 } else { limit },
                tohost,
            )?;
            for (name, value) in &initial_globals {
                cpu.interpreter.globals.insert(name.clone(), value.clone());
            }
            if let Some(initial) = &initial_ram {
                for page in (0..initial.len()).step_by(4096) {
                    let end = (page + 4096).min(initial.len());
                    if cpu.interpreter.memory.bytes()[page..end] != initial[page..end] {
                        cpu.interpreter
                            .memory
                            .write(0x8000_0000 + page as u64, &initial[page..end])?;
                    }
                }
            }
            cpu.steps = 0;
        }
        eprintln!("warmup_specialization={:?}", cpu.specialization_stats);
        cpu.specialization_stats = Default::default();
        if cpu.interpreter.profile.is_some() {
            cpu.interpreter.profile = Some(Default::default());
        }
        warmup_seconds = warm.elapsed().as_secs_f64();
    }
    let operations = cpu.interpreter.operations;
    let start = std::time::Instant::now();
    execute(&mut cpu, limit, tohost)?;
    let seconds = start.elapsed().as_secs_f64();
    if cpu.specialize {
        eprintln!("specialization={:?}", cpu.specialization_stats);
        let stats = &cpu.specialization_stats;
        if stats.training_steps + stats.hits + stats.exits != cpu.steps {
            return Err("specialized instruction accounting mismatch".into());
        }
    }
    if let Some(iterations) = iterations
        && (cpu.gpr(1)? != iterations || cpu.gpr(2)? != 0 || cpu.pc()? != 0x8000_000c)
    {
        return Err("loop result mismatch".into());
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
            "warmup_seconds": warmup_seconds,
            "native_instructions": cpu.specialization_stats.hits,
            "training_instructions": cpu.specialization_stats.training_steps,
            "slow_continuations": cpu.specialization_stats.exits,
            "cache_evictions": cpu.specialization_stats.evictions,
            "ir_operations": cpu.interpreter.operations - operations,
            "functions": profile,
        })
    );
    Ok(())
}
