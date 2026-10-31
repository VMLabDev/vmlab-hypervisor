use vmlab_hv_tcg::{FlatMemory, Memory, Riscv64};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let iterations = std::env::args().nth(1).map_or(Ok(1000u64), |s| s.parse())?;
    if iterations == 0 {
        return Err("iterations must be positive".into());
    }
    let mut memory = FlatMemory::new(0x8000_0000, vec![0; 4096]);
    for (i, instruction) in [0x00108093u32, 0xfff10113, 0xfe011ce3].iter().enumerate() {
        memory.write(0x8000_0000 + i as u64 * 4, &instruction.to_le_bytes())?;
    }
    let mut cpu = Riscv64::new(memory)?;
    cpu.set_pc(0x8000_0000);
    cpu.set_gpr(2, iterations)?;
    cpu.interpreter.operation_limit = u64::MAX;
    cpu.interpreter.memoize = !std::env::args().any(|arg| arg == "--no-memo");
    cpu.interpreter.indexed = !std::env::args().any(|arg| arg == "--no-indexed");
    cpu.specialize = std::env::args().any(|arg| arg == "--specialize");
    if std::env::args().any(|arg| arg == "--profile") {
        cpu.interpreter.profile = Some(Default::default());
    }
    if std::env::args().any(|arg| arg == "--warm") {
        cpu.set_gpr(1, 1_000_000)?;
        cpu.set_gpr(2, 1_000_000)?;
        let warm = std::time::Instant::now();
        cpu.run(65_536)?;
        eprintln!("warmup_seconds={:.6}", warm.elapsed().as_secs_f64());
        cpu.set_pc(0x8000_0000);
        cpu.set_gpr(1, 0)?;
        cpu.set_gpr(2, iterations)?;
    }
    let start_steps = cpu.steps;
    let start_operations = cpu.interpreter.operations;
    let start = std::time::Instant::now();
    cpu.run(iterations * 3)?;
    let elapsed = start.elapsed().as_secs_f64();
    assert_eq!(cpu.gpr(1)?, iterations);
    assert_eq!(cpu.gpr(2)?, 0);
    assert_eq!(cpu.pc()?, 0x8000_000c);
    println!(
        "instructions={} seconds={elapsed:.6} MIPS={:.6} IR_operations={}",
        cpu.steps - start_steps,
        (cpu.steps - start_steps) as f64 / elapsed / 1e6,
        cpu.interpreter.operations - start_operations
    );
    if cpu.specialize {
        println!("specialization={:?}", cpu.specialization_stats);
    }
    if let Some(profile) = cpu.interpreter.profile {
        let mut functions: Vec<_> = profile.functions.into_iter().collect();
        functions.sort_by_key(|(_, (_, own, _))| std::cmp::Reverse(*own));
        for (name, (calls, own, inclusive)) in functions.into_iter().take(30) {
            println!("{name}: calls={calls} own={own} inclusive={inclusive}");
        }
    }
    Ok(())
}
