fn main() {
    let git = |args: &[&str]| {
        std::process::Command::new("git")
            .args(args)
            .output()
            .ok()
            .filter(|output| output.status.success())
            .map(|output| String::from_utf8_lossy(&output.stdout).trim().to_owned())
    };
    let revision = git(&["describe", "--always", "--dirty", "--abbrev=40"])
        .unwrap_or_else(|| "unknown".into());
    println!("cargo:rustc-env=VMLAB_HV_GIT_REV={revision}");
    println!("cargo:rerun-if-changed=../../.git");
    for source in [
        "src",
        "../vmlab-hv/src",
        "../vmlab-hv-proto/src",
        "../vmlab-hv-devices/src",
    ] {
        println!("cargo:rerun-if-changed={source}");
    }
    if let Some(path) = git(&["rev-parse", "--git-path", "HEAD"]) {
        println!("cargo:rerun-if-changed={path}");
    }
    if let Some(branch) = git(&["symbolic-ref", "--quiet", "HEAD"])
        && let Some(path) = git(&["rev-parse", "--git-path", &branch])
    {
        println!("cargo:rerun-if-changed={path}");
    }
}
