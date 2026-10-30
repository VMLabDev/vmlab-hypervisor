//! Binary selection must not reuse an executable from an older checkout.

use std::os::unix::fs::PermissionsExt;
use std::process::Command;

#[test]
fn existing_binary_is_rebuilt_once() {
    if std::env::var_os("VMLAB_HV_BINARY_PROBE").is_some() {
        let first = vmlab_hv_tests::hv_binary();
        assert_eq!(std::fs::read_to_string(&first).unwrap(), "fresh");
        assert_eq!(vmlab_hv_tests::hv_binary(), first);
        return;
    }

    let output = Command::new("mktemp").arg("-d").output().unwrap();
    assert!(output.status.success());
    let dir = std::path::PathBuf::from(String::from_utf8(output.stdout).unwrap().trim());
    std::fs::create_dir(dir.join("debug")).unwrap();
    std::fs::write(dir.join("debug/vmlab-hv"), "stale").unwrap();
    let cargo = dir.join("cargo");
    std::fs::write(
        &cargo,
        "#!/bin/sh\nset -eu\ntest \"$*\" = 'build -p vmlab-hv'\nprintf 'build\\n' >> \"$CARGO_TARGET_DIR/builds\"\nprintf fresh > \"$CARGO_TARGET_DIR/debug/vmlab-hv\"\n",
    ).unwrap();
    std::fs::set_permissions(&cargo, std::fs::Permissions::from_mode(0o700)).unwrap();
    let result = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "existing_binary_is_rebuilt_once", "--nocapture"])
        .env("VMLAB_HV_BINARY_PROBE", "1")
        .env("CARGO_TARGET_DIR", &dir)
        .env("CARGO", &cargo)
        .env_remove("VMLAB_HV_BIN")
        .output()
        .unwrap();
    let builds = std::fs::read_to_string(dir.join("builds")).unwrap_or_default();
    std::fs::remove_dir_all(&dir).unwrap();
    assert!(
        result.status.success(),
        "{}{}",
        String::from_utf8_lossy(&result.stdout),
        String::from_utf8_lossy(&result.stderr)
    );
    assert_eq!(builds, "build\n");
}
