fn main() -> Result<(), Box<dyn std::error::Error>> {
    let path = std::env::args().nth(1).ok_or("usage: roundtrip MODEL.ir")?;
    let source = std::fs::read_to_string(path)?;
    let start = std::time::Instant::now();
    let document = vmlab_hv_sail_ir::parse(&source)?;
    assert_eq!(document.to_string(), source);
    assert_eq!(
        vmlab_hv_sail_ir::parse(&document.to_string())?.definitions,
        document.definitions
    );
    println!(
        "{} bytes, {} definitions, {:?}",
        source.len(),
        document.definitions.len(),
        start.elapsed()
    );
    Ok(())
}
