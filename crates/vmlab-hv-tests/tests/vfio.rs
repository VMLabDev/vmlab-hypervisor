//! S1: explicit, pre-bound VFIO assignment on the Ubuntu 26.04 template.

use serde_json::json;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};
use vmlab_hv_proto::client::Client;
use vmlab_hv_proto::control::{Command as Ctrl, Outcome};
use vmlab_hv_tests::{agent::AgentClient, process::ManagedChild};

#[test]
fn ubuntu_sees_designated_vfio_function_and_refuses_online_snapshots() {
    let Ok(address) = std::env::var("VMLAB_HV_TEST_VFIO_DEVICE") else {
        eprintln!("SKIP VFIO S1: VMLAB_HV_TEST_VFIO_DEVICE is unset; no host device is claimed");
        return;
    };
    // Parse through the machine type and validation before any host path use.
    let firmware = Path::new("/usr/share/qemu/bios-256k.bin");
    if !Path::new("/dev/kvm").exists() || !firmware.is_file() {
        eprintln!("SKIP VFIO S1: /dev/kvm or SeaBIOS is absent");
        return;
    }
    let template = std::env::var_os("VMLAB_HV_TEST_VM_DISK")
        .map(PathBuf::from)
        .or_else(|| {
            let root = PathBuf::from(std::env::var_os("HOME")?)
                .join(".local/share/vmlab/templates/x86_64/ubuntu-26.04");
            let mut paths: Vec<_> = std::fs::read_dir(root)
                .ok()?
                .flatten()
                .map(|e| e.path().join("disk.qcow2"))
                .filter(|p| p.is_file())
                .collect();
            paths.sort();
            paths.pop()
        });
    let Some(template) = template else {
        eprintln!("SKIP VFIO S1: ubuntu-26.04 template absent; set VMLAB_HV_TEST_VM_DISK");
        return;
    };
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path();
    let overlay = path.join("disk.qcow2");
    vmlab_disk::ops::create_linked_clone(&template, &overlay).unwrap();
    let description: vmlab_hv_proto::machine::MachineDescription = serde_json::from_value(json!({
        "name": "vfio-s1", "machine": "q35", "arch": "x86_64", "accel": "kvm",
        "vcpus": 2, "memory_mib": 4096,
        "boot": {"kind": "firmware", "firmware": "seabios", "code": firmware,
            "bootorder": ["/pci@i0cf8/scsi@2/disk@0,0"]},
        "console_pci": {"bus": 0, "device": 1, "function": 0},
        "vfio": [{"host": address, "pci": {"bus": 0, "device": 6, "function": 0}}],
        "disks": [{"id": "disk0", "path": overlay, "format": "qcow2", "read_only": false,
            "vmstate": true, "pci": {"bus": 0, "device": 2, "function": 0}}],
        "nics": [], "serial_log": path.join("serial.log"),
        "ports": [{"name": "vmlab.agent.0", "socket": path.join("agent.sock")}]
    }))
    .unwrap();
    std::fs::write(
        path.join("machine.json"),
        serde_json::to_vec(&description).unwrap(),
    )
    .unwrap();
    let log = std::fs::File::create(path.join("hv.log")).unwrap();
    let mut child = ManagedChild::spawn(
        Command::new("systemd-run")
            .args([
                "--user",
                "--scope",
                "-q",
                "-p",
                "MemoryMax=24G",
                "-p",
                "MemorySwapMax=0",
                "--",
            ])
            .arg(vmlab_hv_tests::hv_binary())
            .arg("--machine")
            .arg(path.join("machine.json"))
            .arg("--control")
            .arg(path.join("control.sock"))
            .stdout(Stdio::from(log.try_clone().unwrap()))
            .stderr(Stdio::from(log)),
    )
    .unwrap();
    let deadline = Instant::now() + Duration::from_secs(30);
    let mut control = loop {
        if let Ok(client) = Client::connect(path.join("control.sock")) {
            break client;
        }
        assert!(
            Instant::now() < deadline && child.try_wait().unwrap().is_none(),
            "VFIO launch failed: {}",
            std::fs::read_to_string(path.join("hv.log")).unwrap()
        );
        std::thread::sleep(Duration::from_millis(25));
    };
    let mut agent =
        AgentClient::connect(&path.join("agent.sock"), Duration::from_secs(180)).unwrap();
    let output = agent
        .exec(&["lspci", "-n", "-s", "00:06.0"], Duration::from_secs(30))
        .unwrap();
    assert_eq!(output.exit_code, 0);
    let found = String::from_utf8_lossy(&output.stdout);
    assert!(
        found.starts_with("00:06.0 "),
        "missing guest VFIO function: {found}"
    );
    // Only read host identity after the VMM has validated the explicit BDF.
    let host = PathBuf::from("/sys/bus/pci/devices").join(&address);
    let vendor = std::fs::read_to_string(host.join("vendor")).unwrap();
    let device = std::fs::read_to_string(host.join("device")).unwrap();
    assert!(
        found.contains(&format!(
            "{}:{}",
            vendor.trim().trim_start_matches("0x"),
            device.trim().trim_start_matches("0x")
        )),
        "host/guest PCI identity mismatch: {found}"
    );
    for command in [
        Ctrl::SnapshotSave {
            name: "assigned".into(),
        },
        Ctrl::SnapshotLoad {
            name: "assigned".into(),
        },
    ] {
        let outcome = control.call(command).unwrap();
        assert!(
            !matches!(outcome, Outcome::Ok { .. }),
            "snapshot unexpectedly accepted"
        );
        assert!(format!("{outcome:?}").contains("VFIO"));
    }
    assert!(matches!(
        control.call(Ctrl::Quit).unwrap(),
        Outcome::Ok { .. }
    ));
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            assert!(status.success());
            break;
        }
        assert!(Instant::now() < deadline, "VFIO VM failed to quit");
        std::thread::sleep(Duration::from_millis(25));
    }
}
