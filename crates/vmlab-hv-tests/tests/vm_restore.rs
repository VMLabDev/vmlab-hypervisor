//! S1/P1: restore a firmware-booted VM during filesystem writes.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};
use vmlab_hv_tests::process::ManagedChild;

use serde_json::json;
use vmlab_hv_proto::client::Client;
use vmlab_hv_proto::control::{Command as Ctrl, Outcome};
use vmlab_hv_proto::machine::MachineDescription;
use vmlab_hv_tests::agent::{AgentClient, ExecOutput};

struct Vm {
    child: ManagedChild,
    dir: tempfile::TempDir,
    control: Client,
}

impl Drop for Vm {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        if std::thread::panicking() {
            self.dir.disable_cleanup(true);
            eprintln!("retained failing VM at {}", self.dir.path().display());
        }
    }
}

impl Vm {
    fn diagnostics(&self) -> String {
        ["hv.log", "serial.log"]
            .map(|name| {
                format!(
                    "{name}: {}",
                    std::fs::read_to_string(self.dir.path().join(name)).unwrap_or_default()
                )
            })
            .join("\n")
    }

    fn agent(&self) -> AgentClient {
        AgentClient::connect(
            &self.dir.path().join("agent.sock"),
            Duration::from_secs(120),
        )
        .unwrap_or_else(|e| panic!("{e}\n{}", self.diagnostics()))
    }

    fn call(&mut self, command: Ctrl) {
        let reply = self.control.call(command).unwrap();
        assert!(
            matches!(reply, Outcome::Ok { .. }),
            "{reply:?}\n{}",
            self.diagnostics()
        );
    }

    fn exec(&self, agent: &mut AgentClient, script: &str) -> ExecOutput {
        self.run(agent, &["sh", "-ec", script])
    }

    fn run(&self, agent: &mut AgentClient, argv: &[&str]) -> ExecOutput {
        let output = agent
            .exec(argv, Duration::from_secs(120))
            .unwrap_or_else(|e| panic!("{e}\n{}", self.diagnostics()));
        assert_eq!(
            output.exit_code,
            0,
            "{}: stdout={} stderr={}\n{}",
            argv[0],
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
            self.diagnostics()
        );
        output
    }
}

fn template() -> Option<PathBuf> {
    if let Some(path) = std::env::var_os("VMLAB_HV_TEST_VM_DISK") {
        let path = PathBuf::from(path);
        assert!(path.is_file(), "VMLAB_HV_TEST_VM_DISK={path:?} is missing");
        return Some(path);
    }
    let root = PathBuf::from(std::env::var_os("HOME")?)
        .join(".local/share/vmlab/templates/x86_64/ubuntu-26.04");
    let mut versions: Vec<_> = std::fs::read_dir(root)
        .ok()?
        .flatten()
        .map(|entry| entry.path().join("disk.qcow2"))
        .filter(|path| path.is_file())
        .collect();
    versions.sort();
    versions.pop()
}

fn launch() -> Option<Vm> {
    let firmware = Path::new("/usr/share/qemu/bios-256k.bin");
    if !Path::new("/dev/kvm").exists()
        || !firmware.is_file()
        || !Path::new("/usr/share/qemu/vgabios-stdvga.bin").is_file()
    {
        eprintln!("SKIP: VM restore requires /dev/kvm, SeaBIOS and SeaVGABIOS");
        return None;
    }
    let Some(template) = template() else {
        eprintln!("SKIP: no ubuntu-26.04 template; set VMLAB_HV_TEST_VM_DISK");
        return None;
    };
    let dir = tempfile::Builder::new()
        .prefix("hv-vm-restore-")
        .tempdir()
        .unwrap();
    let path = dir.path();
    let overlay = path.join("disk.qcow2");
    vmlab_disk::ops::create_linked_clone(&template, &overlay).unwrap();
    let filesystem = path.join("fs.qcow2");
    vmlab_disk::ops::create_qcow2(&filesystem, 128 << 20, None, None).unwrap();
    let description: MachineDescription = serde_json::from_value(json!({
        "name": "vm-restore", "machine": "q35", "arch": "x86_64", "accel": "kvm",
        "vcpus": 2, "memory_mib": 4096,
        "boot": {"kind": "firmware", "firmware": "seabios", "code": firmware,
            "bootorder": ["/pci@i0cf8/scsi@2/disk@0,0"]},
        "console_pci": {"bus": 0, "device": 1, "function": 0},
        "display": {"device": "std-vga", "pci": {"bus": 0, "device": 4, "function": 0}, "rom": null},
        "disks": [{"id": "disk0", "path": overlay, "format": "qcow2",
            "read_only": false, "vmstate": true,
            "pci": {"bus": 0, "device": 2, "function": 0}},
            {"id": "fs", "path": filesystem, "format": "qcow2", "read_only": false,
            "pci": {"bus": 0, "device": 3, "function": 0}}],
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
        Command::new(vmlab_hv_tests::hv_binary())
            .arg("--machine")
            .arg(path.join("machine.json"))
            .arg("--control")
            .arg(path.join("control.sock"))
            .stdout(Stdio::from(log.try_clone().unwrap()))
            .stderr(Stdio::from(log)),
    )
    .unwrap();
    let deadline = Instant::now() + Duration::from_secs(30);
    let control = loop {
        if let Ok(control) = Client::connect(path.join("control.sock")) {
            break control;
        }
        if Instant::now() >= deadline || child.try_wait().unwrap().is_some() {
            let _ = child.kill();
            let _ = child.wait();
            panic!(
                "control unavailable: {}",
                std::fs::read_to_string(path.join("hv.log")).unwrap()
            );
        }
        std::thread::sleep(Duration::from_millis(25));
    };
    Some(Vm {
        child,
        dir,
        control,
    })
}

#[test]
fn vm_snapshot_during_fsync_writes() {
    vm_restore(10);
}

#[test]
fn vm_poweroff_root_filesystem_clean() {
    vm_restore(0);
}

fn vm_restore(iterations: usize) {
    let Some(mut vm) = launch() else { return };
    let mut agent = vm.agent();
    vm.exec(
        &mut agent,
        "mkdir -p /var/tmp/restore-test /mnt/restore-check; mkfs.ext4 -q /dev/vdb; mount /dev/vdb /mnt/restore-check; echo saved > /var/tmp/restore-test/marker; sync",
    );
    for iteration in 0..iterations {
        eprintln!("VM restore iteration {iteration}");
        vm.run(
            &mut agent,
            &["python3", "-c", include_str!("restore_writer.py"), "start"],
        );
        vm.exec(&mut agent, "sleep 1");
        vm.call(Ctrl::SnapshotSave {
            name: "busy".into(),
        });
        vm.exec(
            &mut agent,
            "echo changed > /var/tmp/restore-test/marker; sync; sleep 1",
        );
        vm.call(Ctrl::Pause);
        vm.call(Ctrl::SnapshotLoad {
            name: "busy".into(),
        });
        vm.call(Ctrl::Resume);
        drop(agent);
        agent = vm.agent();
        assert_eq!(
            vm.exec(&mut agent, "cat /var/tmp/restore-test/marker")
                .stdout,
            b"saved\n"
        );
        vm.exec(&mut agent, "sleep 1; touch /var/tmp/restore-test/writable; sync; ! dmesg | grep -Ei 'EXT4-fs error|Aborting journal|Remounting filesystem read-only|Directory block failed checksum'");
        vm.run(
            &mut agent,
            &["python3", "-c", include_str!("restore_writer.py"), "stop"],
        );
        let checked = vm.exec(
            &mut agent,
            "fstrim /; fstrim /mnt/restore-check; umount /mnt/restore-check; e2fsck -fn /dev/vdb; mount /dev/vdb /mnt/restore-check",
        );
        eprintln!(
            "iteration {iteration} fsck: {}{}",
            String::from_utf8_lossy(&checked.stdout),
            String::from_utf8_lossy(&checked.stderr)
        );
        vm.call(Ctrl::SnapshotDelete {
            name: "busy".into(),
        });
    }
    vm.exec(&mut agent, "umount /mnt/restore-check; sync");
    let partition = vm.exec(&mut agent, "dev=$(findmnt -n -o SOURCE /); block=${dev##*/}; printf '[%s,%s]\\n' \"$(cat /sys/class/block/$block/start)\" \"$(cat /sys/class/block/$block/size)\"");
    let [start, sectors]: [u64; 2] = serde_json::from_slice(&partition.stdout).unwrap();
    vm.exec(&mut agent, "systemctl poweroff --no-block");
    let deadline = Instant::now() + Duration::from_secs(60);
    let status = loop {
        if let Some(status) = vm.child.try_wait().unwrap() {
            break status;
        }
        assert!(
            Instant::now() < deadline,
            "poweroff timed out: {}",
            vm.diagnostics()
        );
        std::thread::sleep(Duration::from_millis(25));
    };
    assert!(
        status.success(),
        "guest poweroff failed: {}",
        vm.diagnostics()
    );
    let output = Command::new("qemu-img")
        .arg("check")
        .arg(vm.dir.path().join("disk.qcow2"))
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
    let output = Command::new("qemu-img")
        .arg("check")
        .arg(vm.dir.path().join("fs.qcow2"))
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
    let root = vm.dir.path().join("root.ext4");
    let source = json!({"driver": "raw", "offset": start * 512, "size": sectors * 512,
        "file": {"driver": "qcow2", "file": {"driver": "file", "filename": vm.dir.path().join("disk.qcow2")}}});
    let output = Command::new("qemu-img")
        .args(["convert", "-O", "raw"])
        .arg(format!("json:{source}"))
        .arg(&root)
        .output()
        .unwrap();
    assert!(output.status.success(), "root partition export: {output:?}");
    let output = Command::new("e2fsck")
        .args(["-fn"])
        .arg(root)
        .output()
        .unwrap();
    eprintln!(
        "root fsck: {}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(output.status.success(), "root filesystem check: {output:?}");
}
