fn main() -> anyhow::Result<()> {
    let args: Vec<_> = std::env::args().skip(1).collect();
    if args.iter().any(|arg| arg == "--help" || arg == "-h") {
        println!("{}", vmlab_hv_viewer::Options::HELP);
        return Ok(());
    }
    vmlab_hv_viewer::shell::run(vmlab_hv_viewer::Options::parse(args)?)
}
