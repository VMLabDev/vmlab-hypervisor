fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<_> = std::env::args().skip(1).collect();
    if (3..=4).contains(&args.len()) && args[0] == "replay" {
        let result = vmlab_hv_vmm::virt::benchmark::measure_replay(
            std::path::Path::new(&args[1]),
            args[2].parse()?,
            args.get(3)
                .map(|rounds| rounds.parse())
                .transpose()?
                .unwrap_or(1),
        )?;
        println!("{result}");
        return Ok(());
    }
    if args.len() != 2 {
        return Err("usage: os-workload CASE ROUNDS".into());
    }
    let result = vmlab_hv_vmm::virt::benchmark::measure(&args[0], args[1].parse()?)?;
    println!("{result}");
    Ok(())
}
