//! S1 regression for the BIOS IRQ0 tick used by SYSLINUX's timeout.

use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use vmlab_hv_proto::client::Client;
use vmlab_hv_proto::control::{Command as Control, Outcome};
use vmlab_hv_proto::machine::MachineDescription;

const SHA256: &str = "39fc293200534b03cf805620a099f2f68265220215434fde687536f67a955bc3";

struct Process(Child);
impl Drop for Process {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[test]
fn alpine_syslinux_countdown_reaches_login() {
    let image = vmlab_hv_tests::target_dir().join("s1/alpine-bios.qcow2");
    for path in [
        Path::new("/dev/kvm"),
        Path::new("/usr/share/qemu/bios-256k.bin"),
        Path::new("/usr/share/qemu/vgabios-stdvga.bin"),
        &image,
    ] {
        if !path.exists() {
            eprintln!(
                "skipping Alpine BIOS timer test: {} missing; fetch the image with scripts/fetch-alpine-bios",
                path.display()
            );
            return;
        }
    }
    let hash = Command::new("sha256sum").arg(&image).output().unwrap();
    assert!(hash.status.success());
    assert_eq!(
        String::from_utf8(hash.stdout)
            .unwrap()
            .split_whitespace()
            .next(),
        Some(SHA256)
    );

    let binary = vmlab_hv_tests::hv_binary();
    let dir = tempfile::tempdir().unwrap();
    let overlay = dir.path().join("disk.qcow2");
    vmlab_disk::ops::create_linked_clone(&image, &overlay).unwrap();
    let serial = dir.path().join("serial.log");
    let desc: MachineDescription = serde_json::from_value(serde_json::json!({
        "name": "alpine-bios-timer", "machine": "q35", "arch": "x86_64",
        "accel": "kvm", "vcpus": 1, "memory_mib": 512,
        "boot": {"kind": "firmware", "firmware": "seabios",
            "code": "/usr/share/qemu/bios-256k.bin", "bootorder": []},
        "display": {"device": "std-vga", "pci": {"bus": 0, "device": 1, "function": 0},
            "rom": "/usr/share/qemu/vgabios-stdvga.bin"},
        "disks": [{"id": "disk0", "path": overlay, "format": "qcow2",
            "pci": {"bus": 0, "device": 2, "function": 0},
            "read_only": false, "vmstate": true}],
        "nics": [], "ports": [], "serial_log": serial
    }))
    .unwrap();
    let machine = dir.path().join("machine.json");
    std::fs::write(&machine, serde_json::to_vec(&desc).unwrap()).unwrap();
    let log_path = dir.path().join("hv.log");
    let log = std::fs::File::create(&log_path).unwrap();
    let socket = dir.path().join("control.sock");
    let mut process = Process(
        Command::new(binary)
            .arg("--machine")
            .arg(machine)
            .arg("--control")
            .arg(&socket)
            .arg("--paused")
            .stdout(Stdio::null())
            .stderr(log)
            .spawn()
            .unwrap(),
    );
    let deadline = Instant::now() + Duration::from_secs(15);
    let mut control = loop {
        if let Ok(client) = Client::connect(&socket) {
            break client;
        }
        assert!(
            Instant::now() < deadline && process.0.try_wait().unwrap().is_none(),
            "control socket unavailable: {}",
            std::fs::read_to_string(&log_path).unwrap()
        );
        std::thread::sleep(Duration::from_millis(20));
    };
    assert!(matches!(
        control.call(Control::Resume).unwrap(),
        Outcome::Ok { .. }
    ));
    let started = Instant::now();
    let mut saw_countdown = false;
    loop {
        let text = std::fs::read_to_string(&serial).unwrap_or_default();
        saw_countdown |= text.contains("booted automatically");
        if text.contains("login:") {
            assert!(
                saw_countdown,
                "Alpine did not exercise the SYSLINUX countdown: {text}"
            );
            eprintln!(
                "Alpine passed SYSLINUX and reached login in {:?}",
                started.elapsed()
            );
            break;
        }
        assert!(
            started.elapsed() < Duration::from_secs(60) && process.0.try_wait().unwrap().is_none(),
            "Alpine did not reach login within 60s; countdown={saw_countdown}\n{text}\nhypervisor: {}",
            std::fs::read_to_string(&log_path).unwrap()
        );
        std::thread::sleep(Duration::from_millis(100));
    }
    assert!(matches!(
        control.call(Control::Quit).unwrap(),
        Outcome::Ok { .. }
    ));
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if let Some(status) = process.0.try_wait().unwrap() {
            assert!(status.success());
            break;
        }
        assert!(Instant::now() < deadline, "hypervisor did not quit");
        std::thread::sleep(Duration::from_millis(20));
    }
}
