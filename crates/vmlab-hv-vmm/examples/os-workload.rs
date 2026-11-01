fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<_> = std::env::args().skip(1).collect();
    if args.len() != 2 {
        return Err("usage: os-workload CASE ROUNDS".into());
    }
    let result = vmlab_hv_vmm::virt::benchmark::measure(&args[0], args[1].parse()?)?;
    println!("{result}");
    Ok(())
}
