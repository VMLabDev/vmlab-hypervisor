//! P1 firmware/template acceptance through the executable's control socket.

use std::fs::File;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use vmlab_hv_proto::{
    client::Client,
    control::{Command as Control, Outcome},
};
use vmlab_hv_tests::agent::AgentClient;

struct Process(Child);
impl Drop for Process {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn template(name: &str, variable: &str) -> Option<PathBuf> {
    if let Some(path) = std::env::var_os(variable) {
        return Some(path.into());
    }
    let root = PathBuf::from(std::env::var_os("HOME")?)
        .join(".local/share/vmlab/templates/x86_64")
        .join(name);
    let mut versions: Vec<_> = std::fs::read_dir(root)
        .ok()?
        .filter_map(Result::ok)
        .map(|entry| entry.path().join("disk.qcow2"))
        .filter(|path| path.is_file())
        .collect();
    versions.sort();
    versions.pop()
}

fn boot_template(
    name: &str,
    variable: &str,
    snapshot: bool,
    bus: vmlab_hv_proto::machine::DiskBus,
) {
    if !Path::new("/dev/kvm").exists() {
        eprintln!("skipping {name}: /dev/kvm is unavailable");
        return;
    }
    if !Path::new("/usr/share/qemu/bios-256k.bin").is_file()
        || !Path::new("/usr/share/qemu/vgabios-stdvga.bin").is_file()
    {
        eprintln!("skipping {name}: SeaBIOS or SeaVGABIOS is not installed");
        return;
    }
    let Some(template) = template(name, variable) else {
        eprintln!("skipping {name}: template is absent; set {variable}");
        return;
    };
    assert!(
        template.is_file(),
        "template {} does not exist",
        template.display()
    );
    let dir = tempfile::Builder::new().prefix("hv-p1-").tempdir().unwrap();
    let disk = dir.path().join("disk.qcow2");
    vmlab_disk::ops::create_linked_clone(&template, &disk).unwrap();
    let control = dir.path().join("control.sock");
    let agent = dir.path().join("agent.sock");
    let serial = dir.path().join("serial.log");
    let stderr = dir.path().join("stderr.log");
    let mut description = serde_json::json!({
        "machine": "q35", "name": name, "arch": "x86_64", "accel": "kvm", "vcpus": 2, "memory_mib": 4096,
        "boot": {"kind":"firmware", "firmware":"seabios", "code":"/usr/share/qemu/bios-256k.bin"},
        "disks":[{"id":"disk0", "pci":{"bus":0,"device":2,"function":0}, "path":disk,"format":"qcow2","read_only":false,"vmstate":true}],
        "console_pci":{"bus":0,"device":1,"function":0},
        "ahci_pci":{"bus":0,"device":31,"function":2},
        "display":{"device":"std-vga","pci":{"bus":0,"device":4,"function":0},"rom":null},
        "tablet":{"bus":0,"device":5,"function":0},
        "ports":[{"name":"vmlab.agent.0", "socket":agent}], "nics":[],"serial_log":serial
    });
    if bus == vmlab_hv_proto::machine::DiskBus::Ahci {
        description["disks"][0]
            .as_object_mut()
            .unwrap()
            .remove("pci");
        description["disks"][0]["bus"] = "ahci".into();
        description["disks"][0]["port"] = 0.into();
    }
    description["disks"]
        .as_array_mut()
        .unwrap()
        .push(serde_json::json!({
            "id": "cdrom0", "bus": "ahci", "port": 2, "cdrom": true,
            "format": "raw", "read_only": true
        }));
    let machine = dir.path().join("machine.json");
    std::fs::write(&machine, serde_json::to_vec(&description).unwrap()).unwrap();
    let binary = std::env::var_os("VMLAB_HV_BINARY")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            std::env::current_exe()
                .unwrap()
                .parent()
                .unwrap()
                .parent()
                .unwrap()
                .join("vmlab-hv")
        });
    let mut process = Process(
        Command::new(&binary)
            .arg("--machine")
            .arg(&machine)
            .arg("--control")
            .arg(&control)
            .arg("--paused")
            .env("VMLAB_HV_DIAGNOSTICS", "1")
            .stdout(Stdio::null())
            .stderr(File::create(&stderr).unwrap())
            .spawn()
            .unwrap(),
    );
    let diagnostics = || {
        format!(
            "stderr: {}\nserial: {}",
            std::fs::read_to_string(&stderr).unwrap_or_default(),
            std::fs::read_to_string(&serial).unwrap_or_default()
        )
    };
    let deadline = Instant::now() + Duration::from_secs(20);
    let mut client = loop {
        if let Ok(client) = Client::connect(&control) {
            break client;
        }
        assert!(
            process.0.try_wait().unwrap().is_none(),
            "{name} exited before control ready: {}",
            diagnostics()
        );
        assert!(
            Instant::now() < deadline,
            "{name} control timeout: {}",
            diagnostics()
        );
        std::thread::sleep(Duration::from_millis(20));
    };
    assert!(matches!(
        client.call(Control::Resume).unwrap(),
        Outcome::Ok { .. }
    ));
    let timeout = std::env::var("VMLAB_HV_TEMPLATE_TIMEOUT")
        .ok()
        .map(|v| v.parse().unwrap())
        .unwrap_or(120);
    let mut guest =
        AgentClient::connect(&agent, Duration::from_secs(timeout)).unwrap_or_else(|e| {
            let _ = client.call(Control::Quit);
            let _ = process.0.wait();
            panic!("{name} agent: {e}; {}", diagnostics())
        });
    let result = guest
        .exec(
            &["/bin/sh", "-c", "printf firmware-agent-ready"],
            Duration::from_secs(20),
        )
        .unwrap();
    assert_eq!(result.exit_code, 0);
    assert_eq!(result.stdout, b"firmware-agent-ready");
    if snapshot {
        guest
            .exec(
                &["/bin/sh", "-c", "printf before >/run/vmlab-hv-snapshot"],
                Duration::from_secs(20),
            )
            .unwrap();
        let saved = client
            .call(Control::SnapshotSave {
                name: "firmware".into(),
            })
            .unwrap();
        assert!(
            matches!(saved, Outcome::Ok { .. }),
            "snapshot save: {saved:?}"
        );
        guest
            .exec(
                &["/bin/sh", "-c", "printf after >/run/vmlab-hv-snapshot"],
                Duration::from_secs(20),
            )
            .unwrap();
        let loaded = client
            .call(Control::SnapshotLoad {
                name: "firmware".into(),
            })
            .unwrap();
        assert!(
            matches!(loaded, Outcome::Ok { .. }),
            "snapshot load: {loaded:?}"
        );
        drop(guest);
        let mut guest = AgentClient::connect(&agent, Duration::from_secs(30)).unwrap();
        let restored = guest
            .exec(&["cat", "/run/vmlab-hv-snapshot"], Duration::from_secs(20))
            .unwrap();
        assert_eq!(restored.stdout, b"before");
    }
    assert!(matches!(
        client.call(Control::Quit).unwrap(),
        Outcome::Ok { .. }
    ));
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if let Some(status) = process.0.try_wait().unwrap() {
            assert!(status.success(), "{name} exit: {}", diagnostics());
            break;
        }
        assert!(Instant::now() < deadline, "{name} did not exit after quit");
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn ubuntu_26_04_firmware_agent_and_snapshot() {
    boot_template(
        "ubuntu-26.04",
        "VMLAB_HV_TEMPLATE_UBUNTU_26_04",
        true,
        vmlab_hv_proto::machine::DiskBus::Virtio,
    );
}

#[test]
fn almalinux_10_firmware_agent() {
    boot_template(
        "almalinux-10",
        "VMLAB_HV_TEMPLATE_ALMALINUX_10",
        false,
        vmlab_hv_proto::machine::DiskBus::Virtio,
    );
}

#[test]
fn ubuntu_26_04_ahci_firmware_agent() {
    boot_template(
        "ubuntu-26.04",
        "VMLAB_HV_TEMPLATE_UBUNTU_26_04",
        false,
        vmlab_hv_proto::machine::DiskBus::Ahci,
    );
}
