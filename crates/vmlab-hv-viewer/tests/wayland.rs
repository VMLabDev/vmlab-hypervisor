//! Native Wayland app identity, using a private compositor and an S1 firmware VM.

use std::fs::{File, read_to_string};
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};
use vmlab_hv_proto::{
    client::Client,
    control::{Command as Control, Outcome},
};
use vmlab_hv_tests::process::ManagedChild;

#[test]
#[cfg_attr(vmlab_ci_no_kvm, ignore = "requires KVM and local firmware")]
fn app_id_on_private_wayland_with_s1_vm() {
    for tool in ["dbus-run-session", "kwin_wayland"] {
        if Command::new(tool).arg("--help").output().is_err() {
            eprintln!("SKIP Wayland icon S1: {tool} unavailable");
            return;
        }
    }
    for path in [
        "/dev/kvm",
        "/usr/share/qemu/bios-256k.bin",
        "/usr/share/qemu/vgabios-virtio.bin",
    ] {
        if !Path::new(path).exists() {
            eprintln!("SKIP Wayland icon S1: {path} unavailable");
            return;
        }
    }
    let directory = tempfile::Builder::new()
        .prefix("viewer-icon-")
        .tempdir_in("/tmp")
        .unwrap();
    let root = directory.path();
    let compositor_log = root.join("kwin.log");
    let mut compositor = ManagedChild::helper(
        Command::new("dbus-run-session")
            .args([
                "--",
                "kwin_wayland",
                "--virtual",
                "--no-lockscreen",
                "--no-global-shortcuts",
                "--socket",
                "wl-viewer-icon",
            ])
            .env_remove("DISPLAY")
            .env_remove("WAYLAND_DISPLAY")
            .env("XDG_RUNTIME_DIR", root)
            .env("XDG_CONFIG_HOME", root.join("config"))
            .env("XDG_CACHE_HOME", root.join("cache"))
            .stdout(Stdio::null())
            .stderr(File::create(&compositor_log).unwrap()),
    )
    .unwrap();
    let deadline = Instant::now() + Duration::from_secs(30);
    while !root.join("wl-viewer-icon").exists() {
        assert!(
            compositor.try_wait().unwrap().is_none(),
            "private KWin exited: {}",
            read_to_string(&compositor_log).unwrap()
        );
        assert!(
            Instant::now() < deadline,
            "private KWin startup timed out: {}",
            read_to_string(&compositor_log).unwrap()
        );
        std::thread::sleep(Duration::from_millis(20));
    }

    let machine = root.join("machine.json");
    let control = root.join("control.sock");
    let display = root.join("display.sock");
    std::fs::write(&machine, serde_json::to_vec(&serde_json::json!({
        "machine":"q35", "name":"viewer-icon", "arch":"x86_64", "accel":"kvm", "vcpus":1, "memory_mib":256,
        "boot":{"kind":"firmware","firmware":"seabios","code":"/usr/share/qemu/bios-256k.bin"},
        "display":{"device":"virtio-vga","pci":{"bus":0,"device":3,"function":0},"socket":display},
        "disks":[], "ports":[], "nics":[], "serial_log":root.join("serial.log")
    })).unwrap()).unwrap();
    let hv_log = root.join("hv.log");
    let mut hv = ManagedChild::spawn(
        Command::new(vmlab_hv_tests::hv_binary())
            .arg("--machine")
            .arg(&machine)
            .arg("--control")
            .arg(&control)
            .stdout(Stdio::null())
            .stderr(File::create(&hv_log).unwrap()),
    )
    .unwrap();
    let deadline = Instant::now() + Duration::from_secs(20);
    let mut client = loop {
        if let Ok(client) = Client::connect(&control) {
            break client;
        }
        assert!(
            hv.try_wait().unwrap().is_none(),
            "S1 VM exited: {}",
            read_to_string(&hv_log).unwrap()
        );
        assert!(Instant::now() < deadline, "S1 control startup timed out");
        std::thread::sleep(Duration::from_millis(20));
    };
    assert!(matches!(
        client.call(Control::Status).unwrap(),
        Outcome::Ok { .. }
    ));
    let viewer_log = root.join("viewer.log");
    let mut viewer = ManagedChild::helper(
        Command::new(env!("CARGO_BIN_EXE_vmlab-hv-viewer"))
            .arg("--display")
            .arg(&display)
            .arg("--shm")
            .env_remove("DISPLAY")
            .env("WAYLAND_DISPLAY", "wl-viewer-icon")
            .env("XDG_RUNTIME_DIR", root)
            .env("WAYLAND_DEBUG", "client")
            .stdout(Stdio::null())
            .stderr(File::create(&viewer_log).unwrap()),
    )
    .unwrap();
    let deadline = Instant::now() + Duration::from_secs(30);
    let trace = loop {
        let trace = read_to_string(&viewer_log).unwrap();
        assert!(
            viewer.try_wait().unwrap().is_none(),
            "viewer exited: {trace}"
        );
        if trace.contains("set_app_id(\"dev.vmlab.viewer\")")
            && trace.contains("click to grab keyboard")
            && trace.contains(".attach(")
        {
            break trace;
        }
        assert!(
            Instant::now() < deadline,
            "Wayland app ID/presentation timed out: {trace}"
        );
        std::thread::sleep(Duration::from_millis(20));
    };
    for line in trace
        .lines()
        .filter(|line| line.contains("set_app_id") || line.contains(".attach("))
    {
        eprintln!("{line}");
    }
    assert!(
        !trace.contains("xdg_toplevel_icon_manager_v1@"),
        "winit 0.30.13 should use desktop-entry lookup"
    );
    assert!(matches!(
        client.call(Control::Quit).unwrap(),
        Outcome::Ok { .. }
    ));
    assert!(hv.wait().unwrap().success());
}
