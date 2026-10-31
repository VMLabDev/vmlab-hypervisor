//! Firmware boot through S1, without KVM or host hardware assignment.

use std::{
    path::Path,
    process::{Command, Stdio},
    time::{Duration, Instant},
};
use vmlab_hv_proto::{
    client::Client,
    control::{Command as Control, Outcome, RunState},
};
use vmlab_hv_tests::process::ManagedChild;

#[test]
fn riscv_firmware_boots_opensbi_and_edk2() {
    boot(false, false);
}

#[test]
fn riscv_linux_template_agent_and_snapshot() {
    boot(true, false);
}

#[test]
fn riscv_paused_control_snapshot_round_trip() {
    boot(false, true);
}

fn template() -> Option<std::path::PathBuf> {
    if let Some(path) = std::env::var_os("VMLAB_HV_RISCV_TEMPLATE") {
        return Some(path.into());
    }
    let root = std::path::PathBuf::from(std::env::var_os("HOME")?)
        .join(".local/share/vmlab/templates/riscv64/debian-13");
    let mut candidates: Vec<_> = std::fs::read_dir(root)
        .ok()?
        .filter_map(Result::ok)
        .map(|entry| entry.path().join("disk.qcow2"))
        .filter(|path| path.is_file())
        .collect();
    candidates.sort();
    candidates.pop()
}

fn boot(linux: bool, paused_snapshot: bool) {
    let Some((code, vars)) = vmlab_hv_vmm::virt::firmware_paths() else {
        eprintln!("skipping RISC-V firmware boot: RISCV_VIRT CODE+VARS are unavailable");
        return;
    };
    if !Path::new("/usr/share/qemu/opensbi-riscv64-generic-fw_dynamic.bin").is_file() {
        eprintln!("skipping RISC-V firmware boot: OpenSBI fw_dynamic is unavailable");
        return;
    }
    let mut directory = tempfile::Builder::new()
        .prefix("hv-riscv-")
        .tempdir()
        .unwrap();
    if std::env::var_os("VMLAB_HV_ACCEPTANCE_KEEP").is_some() {
        directory.disable_cleanup(true);
        eprintln!(
            "RISC-V acceptance artifacts: {}",
            directory.path().display()
        );
    }
    let vars_copy = directory.path().join("vars.fd");
    std::fs::copy(vars, &vars_copy).unwrap();
    let serial = directory.path().join("serial.log");
    let control = directory.path().join("control.sock");
    let machine = directory.path().join("machine.json");
    let mut description = serde_json::json!({
        "name": "vmlab:p3b/firmware", "arch": "riscv64", "machine": "virt", "accel": "tcg",
        "vcpus": 1, "memory_mib": 256,
        "boot": { "kind": "firmware", "firmware": "edk2_riscv", "code": code, "vars": vars_copy },
        "disks": [], "nics": [], "ports": [], "serial_log": serial,
    });
    if paused_snapshot {
        let disk = directory.path().join("state.qcow2");
        vmlab_disk::ops::create_qcow2(&disk, 32 << 20, None, None).unwrap();
        description["disks"] = serde_json::json!([{ "id": "state", "pci": { "bus": 0, "device": 2, "function": 0 }, "path": disk, "format": "qcow2", "read_only": false, "vmstate": true }]);
    }
    if linux {
        let Some(template) = template() else {
            eprintln!("skipping RISC-V Linux: Debian template is unavailable");
            return;
        };
        let overlay = directory.path().join("root.qcow2");
        vmlab_disk::ops::create_linked_clone(&template, &overlay).unwrap();
        description["vcpus"] = 2.into();
        description["memory_mib"] = 2048.into();
        description["console_pci"] = serde_json::json!({ "bus": 0, "device": 1, "function": 0 });
        description["ports"] = serde_json::json!([{ "name": "vmlab.agent.0", "socket": directory.path().join("agent.sock") }]);
        description["disks"] = serde_json::json!([{ "id": "root", "pci": { "bus": 0, "device": 2, "function": 0 }, "path": overlay, "format": "qcow2", "read_only": false, "vmstate": true }]);
        description["display"] = serde_json::json!({ "device": "virtio-gpu", "pci": { "bus": 0, "device": 4, "function": 0 }, "rom": null });
        description["tablet"] = serde_json::json!({ "bus": 0, "device": 5, "function": 0 });
    }
    std::fs::write(&machine, serde_json::to_vec(&description).unwrap()).unwrap();
    let timeout = Duration::from_secs(
        std::env::var("VMLAB_HV_RISCV_BOOT_TIMEOUT")
            .unwrap_or_else(|_| if linux { "3600" } else { "1800" }.into())
            .parse()
            .unwrap(),
    );
    let stderr = directory.path().join("stderr.log");
    let mut process = ManagedChild::spawn_with_timeout(
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
            .arg(&machine)
            .arg("--control")
            .arg(&control)
            .arg("--paused")
            .stdout(Stdio::null())
            .stderr(std::fs::File::create(&stderr).unwrap()),
        timeout + Duration::from_secs(300),
    )
    .unwrap();
    let start = Instant::now();
    let diagnostics = || {
        format!(
            "serial:\n{}\nstderr:\n{}",
            std::fs::read_to_string(&serial).unwrap_or_default(),
            std::fs::read_to_string(&stderr).unwrap_or_default()
        )
    };
    while !control.exists() {
        assert!(process.try_wait().unwrap().is_none(), "{}", diagnostics());
        assert!(
            start.elapsed() < timeout,
            "control socket timeout: {}",
            diagnostics()
        );
        std::thread::sleep(Duration::from_millis(25));
    }
    let mut client = Client::connect(&control).unwrap();
    assert!(matches!(
        client.call(Control::Status).unwrap(),
        Outcome::Ok {
            status: Some(RunState::Paused)
        }
    ));
    if paused_snapshot {
        for command in [
            Control::SnapshotSave {
                name: "paused".into(),
            },
            Control::SnapshotLoad {
                name: "paused".into(),
            },
            Control::SnapshotDelete {
                name: "paused".into(),
            },
        ] {
            let outcome = client.call(command).unwrap();
            assert!(
                matches!(
                    outcome,
                    Outcome::Ok {
                        status: Some(RunState::Paused)
                    }
                ),
                "{outcome:?}\n{}",
                diagnostics()
            );
        }
        assert!(std::fs::read_to_string(&serial).unwrap().is_empty());
        client.call(Control::Quit).unwrap();
        assert!(process.wait().unwrap().success(), "{}", diagnostics());
        return;
    }
    assert!(matches!(
        client.call(Control::Resume).unwrap(),
        Outcome::Ok { .. }
    ));
    let mut last_progress = Instant::now();
    loop {
        let text = std::fs::read_to_string(&serial).unwrap_or_default();
        if text.contains("OpenSBI") && text.contains("RISC-V EDK2 firmware") {
            break;
        }
        assert!(process.try_wait().unwrap().is_none(), "{}", diagnostics());
        assert!(
            start.elapsed() < timeout,
            "firmware boot timeout: {}",
            diagnostics()
        );
        if last_progress.elapsed() > Duration::from_secs(30) {
            eprintln!(
                "RISC-V firmware at {:.1}s, {} serial bytes",
                start.elapsed().as_secs_f64(),
                text.len()
            );
            last_progress = Instant::now();
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    eprintln!(
        "RISC-V firmware reached edk2 in {:.3}s",
        start.elapsed().as_secs_f64()
    );
    if linux {
        use vmlab_hv_tests::agent::AgentClient;
        let agent_path = directory.path().join("agent.sock");
        let mut agent = AgentClient::connect(&agent_path, timeout.saturating_sub(start.elapsed()))
            .unwrap_or_else(|error| panic!("agent: {error}\n{}", diagnostics()));
        eprintln!(
            "RISC-V boot to agent-ready: {:.3}s",
            start.elapsed().as_secs_f64()
        );
        let before = agent
            .exec(
                &["/bin/sh", "-c", "printf before >/run/p3b-snapshot"],
                Duration::from_secs(60),
            )
            .unwrap();
        assert_eq!(before.exit_code, 0);
        assert!(matches!(
            client
                .call(Control::SnapshotSave {
                    name: "booted".into()
                })
                .unwrap(),
            Outcome::Ok { .. }
        ));
        let after = agent
            .exec(
                &["/bin/sh", "-c", "printf after >/run/p3b-snapshot"],
                Duration::from_secs(60),
            )
            .unwrap();
        assert_eq!(after.exit_code, 0);
        drop(agent);
        assert!(matches!(
            client
                .call(Control::SnapshotLoad {
                    name: "booted".into()
                })
                .unwrap(),
            Outcome::Ok { .. }
        ));
        let mut agent = AgentClient::connect(&agent_path, Duration::from_secs(120)).unwrap();
        let restored = agent
            .exec(&["/bin/cat", "/run/p3b-snapshot"], Duration::from_secs(60))
            .unwrap();
        assert_eq!(restored.exit_code, 0);
        assert_eq!(restored.stdout, b"before");
        assert!(matches!(
            client
                .call(Control::SnapshotDelete {
                    name: "booted".into()
                })
                .unwrap(),
            Outcome::Ok { .. }
        ));
    }
    assert!(matches!(
        client.call(Control::Pause).unwrap(),
        Outcome::Ok {
            status: Some(RunState::Paused)
        }
    ));
    client.call(Control::Quit).unwrap();
    assert!(process.wait().unwrap().success(), "{}", diagnostics());
}
