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

#[test]
fn riscv_direct_kernel_receives_hart_and_device_tree_and_powers_off() {
    direct_kernel(false, false);
}

#[test]
fn riscv_direct_kernel_reset_stays_in_process() {
    direct_kernel(true, false);
}

#[test]
fn riscv_direct_kernel_exit_on_reset() {
    direct_kernel(true, true);
}

fn direct_kernel(reset: bool, exit_on_reset: bool) {
    if !Path::new("/usr/share/qemu/opensbi-riscv64-generic-fw_dynamic.bin").is_file() {
        eprintln!("skipping RISC-V direct boot: OpenSBI fw_dynamic is unavailable");
        return;
    }
    let directory = tempfile::Builder::new()
        .prefix("hv-riscv-direct-")
        .tempdir()
        .unwrap();
    let kernel = directory.path().join("kernel.bin");
    let serial = directory.path().join("serial.log");
    let stderr = directory.path().join("stderr.log");
    let control = directory.path().join("control.sock");
    let machine = directory.path().join("machine.json");
    // Reject a nonzero hart ID or an a1 pointer that does not address an FDT.
    let mut words = vec![0, 0x0005a303, 0xedfe13b7, 0xdd038393, 0, 0x100002b7];
    if reset {
        // Keep a marker outside the boot images. The second boot must see it
        // in the same RAM, and return through OpenSBI with a fresh device tree.
        words.extend([0x80100e37, 0x020e1e13, 0x020e5e13, 0x000e4e83]);
        let branch = words.len();
        words.push(0);
        words.extend([
            0x00100e93, 0x01de0023, 0x001002b7, 0x00007337, 0x77730313, 0x0062a023, 0x0000006f,
        ]);
        words[branch] = branch_ne(branch, words.len(), 29, 0);
    }
    for byte in b"direct-kernel-ok\n" {
        words.extend([
            (*byte as u32) << 20 | 6 << 7 | 0x13,
            6 << 20 | 5 << 15 | 0x23,
        ]);
    }
    words.extend([0x001002b7, 0x00005337, 0x55530313, 0x0062a023, 0x0000006f]);
    let failure = words.len();
    words.extend([0x001002b7, 0x00003337, 0x33330313, 0x0062a023, 0x0000006f]);
    for (index, first, second) in [(0, 10, 0), (4, 6, 7)] {
        words[index] = branch_ne(index, failure, first, second);
    }
    std::fs::write(
        &kernel,
        words
            .iter()
            .flat_map(|word| word.to_le_bytes())
            .collect::<Vec<_>>(),
    )
    .unwrap();
    std::fs::write(
        &machine,
        serde_json::to_vec(&serde_json::json!({
            "name": "vmlab:p3b/direct", "arch": "riscv64", "machine": "virt", "accel": "tcg",
            "vcpus": 1, "memory_mib": 128,
            "exit_on_reset": exit_on_reset,
            "boot": { "kind": "kernel", "kernel": kernel, "cmdline": "console=ttyS0" },
            "disks": [], "nics": [], "ports": [], "serial_log": serial,
        }))
        .unwrap(),
    )
    .unwrap();
    let timeout = Duration::from_secs(1200);
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
            .arg(machine)
            .arg("--control")
            .arg(&control)
            .arg("--paused")
            .stdout(Stdio::null())
            .stderr(std::fs::File::create(&stderr).unwrap()),
        timeout,
    )
    .unwrap();
    let start = Instant::now();
    while !control.exists() {
        assert!(process.try_wait().unwrap().is_none());
        assert!(start.elapsed() < timeout);
        std::thread::sleep(Duration::from_millis(25));
    }
    let mut client = Client::connect(control).unwrap();
    assert!(matches!(
        client.call(Control::Resume).unwrap(),
        Outcome::Ok {
            status: Some(RunState::Running)
        }
    ));
    let status = loop {
        if let Some(status) = process.try_wait().unwrap() {
            break status;
        }
        assert!(start.elapsed() < timeout);
        std::thread::sleep(Duration::from_millis(100));
    };
    let text = std::fs::read_to_string(serial).unwrap_or_default();
    let errors = std::fs::read_to_string(stderr).unwrap_or_default();
    assert!(
        status.success() && text.contains("direct-kernel-ok") != (reset && exit_on_reset),
        "{status}\n{text}\n{errors}"
    );
    let expected = if reset && exit_on_reset {
        vmlab_hv_proto::control::ShutdownReason::GuestReset
    } else {
        vmlab_hv_proto::control::ShutdownReason::GuestShutdown
    };
    let mut shutdown = None;
    let mut resets = 0;
    while let Some(event) = client.wait_event().unwrap() {
        match event {
            vmlab_hv_proto::control::Event::Shutdown { reason } => shutdown = Some(reason),
            vmlab_hv_proto::control::Event::Reset => resets += 1,
            _ => {}
        }
    }
    assert_eq!(shutdown, Some(expected));
    assert_eq!(resets, usize::from(reset && !exit_on_reset));
}

fn branch_ne(index: usize, target: usize, first: u32, second: u32) -> u32 {
    let offset = ((target - index) * 4) as u32;
    (offset >> 12 & 1) << 31
        | (offset >> 5 & 0x3f) << 25
        | second << 20
        | first << 15
        | 1 << 12
        | (offset >> 1 & 15) << 8
        | (offset >> 11 & 1) << 7
        | 0x63
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
    let replay = linux
        .then(|| std::env::var_os("VMLAB_HV_RISCV_REPLAY"))
        .flatten()
        .map(std::path::PathBuf::from);
    let artifacts = replay.as_deref().unwrap_or(directory.path());
    let serial = artifacts.join("serial.log");
    let control = artifacts.join("control.sock");
    let machine = artifacts.join("machine.json");
    if replay.is_some() {
        validate_replay_fixture(artifacts);
        eprintln!(
            "RISC-V diagnostic replay: {}; this does not prove a cold boot",
            artifacts.display()
        );
    } else {
        let vars_copy = directory.path().join("vars.fd");
        std::fs::copy(vars, &vars_copy).unwrap();
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
            description["console_pci"] =
                serde_json::json!({ "bus": 0, "device": 1, "function": 0 });
            description["ports"] = serde_json::json!([{ "name": "vmlab.agent.0", "socket": directory.path().join("agent.sock") }]);
            description["disks"] = serde_json::json!([{ "id": "root", "pci": { "bus": 0, "device": 2, "function": 0 }, "path": overlay, "format": "qcow2", "read_only": false, "vmstate": true }]);
            description["display"] = serde_json::json!({ "device": "virtio-gpu", "pci": { "bus": 0, "device": 4, "function": 0 }, "rom": null });
            description["tablet"] = serde_json::json!({ "bus": 0, "device": 5, "function": 0 });
        }
        std::fs::write(&machine, serde_json::to_vec(&description).unwrap()).unwrap();
    }
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
        client
            .call(if replay.is_some() {
                Control::SnapshotLoad {
                    name: "p3b-replay".into(),
                }
            } else {
                Control::Resume
            })
            .unwrap(),
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
        let agent_path = artifacts.join("agent.sock");
        let mut agent = AgentClient::connect(&agent_path, timeout.saturating_sub(start.elapsed()))
            .unwrap_or_else(|error| panic!("agent: {error}\n{}", diagnostics()));
        eprintln!(
            "RISC-V boot to agent-ready: {:.3}s",
            start.elapsed().as_secs_f64()
        );
        for (name, command) in [
            ("linux.dtb", "cat /sys/firmware/fdt"),
            (
                "pci-config.txt",
                "for d in /sys/bus/pci/devices/*; do printf '\\n--- %s\\n' \"$d\"; od -Ax -tx1z -v -N 256 \"$d/config\" || exit; done",
            ),
            ("interrupts.txt", "cat /proc/interrupts"),
            ("iomem.txt", "cat /proc/iomem"),
            ("cpuinfo.txt", "cat /proc/cpuinfo"),
        ] {
            let captured = agent
                .exec(&["/bin/sh", "-c", command], Duration::from_secs(120))
                .unwrap();
            assert_eq!(captured.exit_code, 0, "{name}: {:?}", captured.stderr);
            assert!(
                !captured.stdout.is_empty(),
                "empty hardware artifact {name}"
            );
            std::fs::write(directory.path().join(name), captured.stdout).unwrap();
        }
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

fn validate_replay_fixture(directory: &Path) {
    use std::os::unix::{fs::FileTypeExt, net::UnixStream};
    use vmlab_hv_proto::machine::{Accel, Arch, Boot, Machine, MachineDescription};
    assert_eq!(directory.canonicalize().unwrap(), directory);
    assert!(directory.starts_with(std::env::temp_dir()));
    assert!(
        directory
            .file_name()
            .unwrap()
            .to_str()
            .unwrap()
            .starts_with("hv-riscv-")
    );
    let desc: MachineDescription =
        serde_json::from_slice(&std::fs::read(directory.join("machine.json")).unwrap()).unwrap();
    assert_eq!(desc.name, "vmlab:p3b/firmware");
    assert_eq!(
        (desc.arch, desc.machine, desc.accel),
        (Arch::Riscv64, Machine::Virt, Accel::Tcg)
    );
    assert_eq!(desc.serial_log, directory.join("serial.log"));
    assert_eq!(desc.disks.len(), 1);
    assert_eq!(
        desc.disks[0].path.as_deref(),
        Some(directory.join("root.qcow2").as_path())
    );
    assert!(desc.disks[0].vmstate && !desc.disks[0].read_only);
    assert!(
        matches!(desc.boot, Boot::Firmware { vars: Some(ref vars), .. } if *vars == directory.join("vars.fd"))
    );
    assert!(desc.nics.is_empty() && desc.shares.is_empty() && desc.vfio.is_empty());
    assert!(
        desc.ports
            .iter()
            .all(|port| port.socket == directory.join("agent.sock"))
    );
    let mut stale = Vec::new();
    for name in ["control.sock", "display.sock", "vnc.sock", "agent.sock"] {
        let path = directory.join(name);
        match std::fs::symlink_metadata(&path) {
            Ok(metadata) => {
                assert!(
                    metadata.file_type().is_socket(),
                    "not a socket: {}",
                    path.display()
                );
                match UnixStream::connect(&path) {
                    Err(error) if error.kind() == std::io::ErrorKind::ConnectionRefused => {}
                    result => panic!(
                        "replay requires a stopped fixture: {}: {result:?}",
                        path.display()
                    ),
                }
                stale.push(path);
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => panic!("{}: {error}", path.display()),
        }
    }
    for path in stale {
        std::fs::remove_file(path).unwrap();
    }
}
