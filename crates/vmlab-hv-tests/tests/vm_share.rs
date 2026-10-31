//! S1/P1: q35 template guests mount virtio-fs shares.
//!
//! One test serves the share with the in-process device (`path`): the
//! ubuntu-26.04 template mounts it, reads a host file and writes one back.
//! The other serves a share with an external virtiofsd over `socket`: it
//! pins the regression where the vhost-user front-end refused the q35
//! machine's DMA view (the PAM shadow and SMRAM aliases are anonymous
//! overlays inside the 0xa0000–0x100000 hole), which killed every FUSE
//! request after DRIVER_OK.
//!
//! Both skip with a reason when KVM, SeaBIOS or the template is missing.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::json;
use vmlab_hv_proto::client::Client;
use vmlab_hv_proto::control::Command as Ctrl;
use vmlab_hv_proto::machine::MachineDescription;
use vmlab_hv_tests::agent::AgentClient;
use vmlab_hv_tests::process::ManagedChild;

const FIRMWARE: &str = "/usr/share/qemu/bios-256k.bin";
const VIRTIOFSD: &str = "/usr/lib/virtiofsd";

struct Vm {
    child: ManagedChild,
    dir: tempfile::TempDir,
    control: Client,
}

impl Drop for Vm {
    fn drop(&mut self) {
        let _ = self.control.call(Ctrl::Quit);
        let deadline = Instant::now() + Duration::from_secs(20);
        while Instant::now() < deadline {
            match self.child.try_wait() {
                Ok(Some(_)) => return,
                _ => std::thread::sleep(Duration::from_millis(50)),
            }
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Vm {
    fn diagnostics(&self) -> String {
        format!(
            "--- hv.log ---\n{}\n--- fsd.log ---\n{}\n--- serial tail ---\n{}",
            self.read("hv.log"),
            self.read("fsd.log"),
            tail(&self.read("serial.log"), 3000),
        )
    }

    fn read(&self, name: &str) -> String {
        std::fs::read_to_string(self.dir.path().join(name)).unwrap_or_default()
    }

    fn agent(&self) -> AgentClient {
        AgentClient::connect(
            &self.dir.path().join("agent.sock"),
            Duration::from_secs(180),
        )
        .unwrap_or_else(|e| panic!("agent connect: {e}; {}", self.diagnostics()))
    }

    fn exec(&self, agent: &mut AgentClient, script: &str) -> vmlab_hv_tests::agent::ExecOutput {
        let out = agent
            .exec(&["sh", "-ec", script], Duration::from_secs(180))
            .unwrap_or_else(|e| panic!("exec {script:?}: {e}; {}", self.diagnostics()));
        assert!(
            out.exit_code == 0,
            "guest script {script:?} failed with {}:\n{}{}\n{}",
            out.exit_code,
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr),
            self.diagnostics()
        );
        out
    }
}

fn tail(text: &str, chars: usize) -> String {
    text.chars()
        .rev()
        .take(chars)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect()
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

fn available() -> bool {
    Path::new("/dev/kvm").exists() && Path::new(FIRMWARE).is_file() && template().is_some()
}

/// How the machine description names the share.
enum Share {
    /// The in-process device rooted at the share directory.
    Path,
    /// An external virtiofsd this test starts on a socket.
    Socket,
}

/// Boot the q35 template with one share, prepared at `<dir>/share` either
/// way: `Path` serves it with the in-process device, `Socket` with an
/// external virtiofsd.
fn launch(share: Share) -> Option<Vm> {
    if !available() {
        eprintln!("SKIP: q35 share tests need /dev/kvm, SeaBIOS and the ubuntu-26.04 template");
        return None;
    }
    let template = template().unwrap();
    let dir = tempfile::Builder::new()
        .prefix("hv-q35-share-")
        .tempdir()
        .unwrap();
    let path = dir.path();
    let overlay = path.join("disk.qcow2");
    vmlab_disk::ops::create_linked_clone(&template, &overlay).unwrap();
    let share_dir = path.join("share");
    std::fs::create_dir(&share_dir).unwrap();
    std::fs::write(share_dir.join("hello.txt"), b"hello-q35\n").unwrap();

    // An external virtiofsd when the test asks for the socket form.
    let socket = path.join("fs.sock");
    if matches!(share, Share::Socket) {
        let fsd_log = std::fs::File::create(path.join("fsd.log")).unwrap();
        let mut fsd = ManagedChild::helper(
            Command::new(VIRTIOFSD)
                .arg("--socket-path")
                .arg(&socket)
                .arg("--shared-dir")
                .arg(&share_dir)
                .arg("--cache")
                .arg("auto")
                .arg("--log-level")
                .arg("warn")
                .stdout(fsd_log.try_clone().unwrap())
                .stderr(Stdio::from(fsd_log)),
        )
        .unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        while !socket.exists() {
            if let Ok(Some(status)) = fsd.try_wait() {
                panic!("virtiofsd exited at startup with {status}");
            }
            assert!(Instant::now() < deadline, "virtiofsd never bound");
            std::thread::sleep(Duration::from_millis(50));
        }
        // The daemon outlives its front-end; killed when the process ends.
        std::mem::forget(fsd);
    }

    let share = match share {
        Share::Socket => json!({"tag": "scratch", "socket": socket.display().to_string(),
                                "pci": {"bus": 0, "device": 5, "function": 0}}),
        Share::Path => json!({"tag": "scratch", "path": share_dir.display().to_string(),
                              "pci": {"bus": 0, "device": 5, "function": 0}}),
    };
    let description: MachineDescription = serde_json::from_value(json!({
        "name": "q35-share", "machine": "q35", "arch": "x86_64", "accel": "kvm",
        "vcpus": 2, "memory_mib": 4096,
        "boot": {"kind": "firmware", "firmware": "seabios", "code": FIRMWARE,
                 "bootorder": ["/pci@i0cf8/scsi@2/disk@0,0"]},
        "console_pci": {"bus": 0, "device": 1, "function": 0},
        "display": {"device": "std-vga", "pci": {"bus": 0, "device": 4, "function": 0}, "rom": null},
        "disks": [{"id": "disk0", "path": overlay, "format": "qcow2",
                   "read_only": false, "vmstate": true,
                   "pci": {"bus": 0, "device": 2, "function": 0}}],
        "nics": [], "serial_log": path.join("serial.log"),
        "ports": [{"name": "vmlab.agent.0", "socket": path.join("agent.sock")}],
        "shares": [share]
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

/// Mount the share, read the host's file and write one back.
fn mount_and_exchange(vm: &Vm, agent: &mut AgentClient) {
    vm.exec(
        agent,
        "mkdir -p /mnt/share; mount -t virtiofs scratch /mnt/share; \
         grep -q hello-q35 /mnt/share/hello.txt; \
         echo written-in-guest > /mnt/share/guest.txt; sync",
    );
    assert_eq!(
        std::fs::read_to_string(vm.dir.path().join("share/guest.txt")).unwrap(),
        "written-in-guest\n"
    );
    vm.exec(agent, "umount /mnt/share");
}

#[test]
fn q35_template_mounts_an_in_process_share() {
    let Some(mut vm) = launch(Share::Path) else {
        return;
    };
    let mut agent = vm.agent();
    mount_and_exchange(&vm, &mut agent);
    let _ = vm.control.call(Ctrl::Quit);
}

#[test]
fn q35_template_mounts_an_external_virtiofsd_share() {
    // Regression: the vhost-user front-end used to refuse the q35 machine's
    // DMA view (anonymous PAM/SMRAM overlays) and the guest's first FUSE
    // request then hung forever.
    let Some(mut vm) = launch(Share::Socket) else {
        return;
    };
    let mut agent = vm.agent();
    mount_and_exchange(&vm, &mut agent);
    let _ = vm.control.call(Ctrl::Quit);
}
