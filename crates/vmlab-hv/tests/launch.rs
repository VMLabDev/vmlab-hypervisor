//! Launch-contract tests: spawn the `vmlab-hv` binary and drive it over the
//! control socket, as vmlab will. These are the S1-adjacent checks this work
//! package owns: the process contract (bind, run, exit codes) and the
//! command surface. The kernel asset is vmlab's own container kernel; the
//! tests skip with a printed reason when it or `/dev/kvm` is missing.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use vmlab_hv_proto::client::Client;
use vmlab_hv_proto::control::{Command as Ctrl, Event, Outcome, RunState, ShutdownReason};

const KERNEL: &str = "/home/wil/dev/vmlab/guest/dist/x86_64/vmlinuz";

fn kernel_available() -> bool {
    Path::new("/dev/kvm").exists() && Path::new(KERNEL).exists()
}

struct Dir {
    path: PathBuf,
}

impl Dir {
    fn new(tag: &str) -> Dir {
        let path = std::env::temp_dir().join(format!(
            "vmlab-hv-launch-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_millis()
        ));
        std::fs::create_dir_all(&path).unwrap();
        Dir { path }
    }

    fn machine(&self) -> PathBuf {
        let json = format!(
            r#"{{
                "name": "launch-test",
                "arch": "x86_64",
                "accel": "kvm",
                "vcpus": 1,
                "memory_mib": 256,
                "boot": {{ "kind": "kernel", "kernel": "{KERNEL}", "initrd": null,
                          "cmdline": "console=ttyS0 panic=-1 reboot=t" }},
                "disks": [],
                "nics": [],
                "serial_log": "{}",
                "ports": [],
                "shares": []
            }}"#,
            self.serial_log().display(),
        );
        let path = self.path.join("machine.json");
        std::fs::write(&path, json).unwrap();
        path
    }

    fn serial_log(&self) -> PathBuf {
        self.path.join("serial.log")
    }

    fn control(&self) -> PathBuf {
        self.path.join("control.sock")
    }
}

impl Drop for Dir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

fn wait_for_exit(mut child: std::process::Child, timeout: Duration) -> std::process::ExitStatus {
    let deadline = Instant::now() + timeout;
    loop {
        match child.try_wait().unwrap() {
            Some(status) => return status,
            None if Instant::now() > deadline => {
                let _ = child.kill();
                panic!("vmlab-hv did not exit within {timeout:?}");
            }
            None => std::thread::sleep(Duration::from_millis(50)),
        }
    }
}

fn serial_log_text(dir: &Dir) -> String {
    std::fs::read_to_string(dir.serial_log()).unwrap_or_default()
}

#[test]
fn boots_kernel_and_exits_zero_on_guest_reset() {
    if !kernel_available() {
        eprintln!("skipping: /dev/kvm or the container kernel is not present");
        return;
    }
    let dir = Dir::new("reset");
    let machine = dir.machine();

    let child = Command::new(env!("CARGO_BIN_EXE_vmlab-hv"))
        .arg("--machine")
        .arg(&machine)
        .arg("--control")
        .arg(dir.control())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn vmlab-hv");

    let status = wait_for_exit(child, Duration::from_secs(60));
    assert!(status.success(), "guest reset must exit 0: {status}");
    let log = serial_log_text(&dir);
    assert!(
        log.contains("Linux version"),
        "the kernel must write its banner to the serial log: {log}"
    );
}

#[test]
fn paused_machine_resumes_over_the_control_socket() {
    if !kernel_available() {
        eprintln!("skipping: /dev/kvm or the container kernel is not present");
        return;
    }
    let dir = Dir::new("paused");
    let machine = dir.machine();

    let mut child = Command::new(env!("CARGO_BIN_EXE_vmlab-hv"))
        .arg("--machine")
        .arg(&machine)
        .arg("--control")
        .arg(dir.control())
        .arg("--paused")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn vmlab-hv");

    // The control socket exists before the vCPUs run.
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut client = loop {
        if let Ok(c) = Client::connect(dir.control()) {
            break c;
        }
        if Instant::now() > deadline {
            let _ = child.kill();
            panic!("control socket never became connectable");
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    assert_eq!(client.proto_version(), vmlab_hv_proto::PROTO_VERSION);

    match client.call(Ctrl::Status).unwrap() {
        Outcome::Ok {
            status: Some(RunState::Paused),
        } => {}
        other => panic!("expected paused, got {other:?}"),
    }

    // A paused machine writes nothing to the serial log.
    std::thread::sleep(Duration::from_millis(300));
    assert!(
        serial_log_text(&dir).is_empty(),
        "paused vCPUs must not run"
    );

    match client.call(Ctrl::Resume).unwrap() {
        Outcome::Ok {
            status: Some(RunState::Running),
        } => {}
        other => panic!("expected running, got {other:?}"),
    }

    // The `resumed` event arrives first, then the kernel reaches its panic
    // (no rootfs); with panic=-1 that is a reset: the shutdown event arrives
    // and the process exits 0.
    let mut saw_resumed = false;
    let shutdown = loop {
        match client.wait_event().unwrap().expect("events before EOF") {
            Event::Resumed => saw_resumed = true,
            Event::Shutdown { reason } => break reason,
            other => panic!("unexpected event {other:?}"),
        }
    };
    assert!(saw_resumed, "resume must be reported as an event");
    assert_eq!(shutdown, ShutdownReason::GuestReset);
    let status = wait_for_exit(child, Duration::from_secs(30));
    assert!(status.success(), "{status}");
    assert!(serial_log_text(&dir).contains("Linux version"));
}

#[test]
fn quit_stops_the_machine_with_exit_zero() {
    if !kernel_available() {
        eprintln!("skipping: /dev/kvm or the container kernel is not present");
        return;
    }
    let dir = Dir::new("quit");
    let machine = dir.machine();

    let mut child = Command::new(env!("CARGO_BIN_EXE_vmlab-hv"))
        .arg("--machine")
        .arg(&machine)
        .arg("--control")
        .arg(dir.control())
        .arg("--paused")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn vmlab-hv");

    let deadline = Instant::now() + Duration::from_secs(10);
    let mut client = loop {
        if let Ok(c) = Client::connect(dir.control()) {
            break c;
        }
        if Instant::now() > deadline {
            let _ = child.kill();
            panic!("control socket never became connectable");
        }
        std::thread::sleep(Duration::from_millis(20));
    };

    match client.call(Ctrl::Quit).unwrap() {
        Outcome::Ok { .. } => {}
        other => panic!("quit must succeed, got {other:?}"),
    }
    match client.wait_event().unwrap() {
        Some(Event::Shutdown {
            reason: ShutdownReason::HostRequest,
        }) => {}
        other => panic!("expected host_request shutdown, got {other:?}"),
    }
    let status = wait_for_exit(child, Duration::from_secs(10));
    assert!(status.success(), "a requested shutdown exits 0: {status}");
}

#[test]
fn virtio_devices_are_enumerated_through_the_pci_root() {
    if !kernel_available() {
        eprintln!("skipping: /dev/kvm or the container kernel is not present");
        return;
    }
    let dir = Dir::new("virtio");
    // A blank raw disk and one console port: the kernel must find the
    // virtio-pci functions behind this VMM's CF8/CFC and ECAM root.
    let disk = dir.path.join("rootfs.raw");
    std::fs::write(&disk, vec![0u8; 1024 * 1024]).unwrap();
    let port = dir.path.join("agent.sock");
    let json = format!(
        r#"{{
            "name": "virtio-test",
            "arch": "x86_64",
            "accel": "kvm",
            "vcpus": 1,
            "memory_mib": 256,
            "boot": {{ "kind": "kernel", "kernel": "{KERNEL}", "initrd": null,
                      "cmdline": "console=ttyS0 panic=-1 reboot=t" }},
            "disks": [{{ "id": "rootfs", "path": "{}", "format": "raw",
                         "read_only": true, "vmstate": false }}],
            "nics": [],
            "serial_log": "{}",
            "ports": [{{ "name": "vmlab.agent.0", "socket": "{}" }}],
            "shares": []
        }}"#,
        disk.display(),
        dir.serial_log().display(),
        port.display(),
    );
    let machine = dir.path.join("machine.json");
    std::fs::write(&machine, json).unwrap();

    let child = Command::new(env!("CARGO_BIN_EXE_vmlab-hv"))
        .arg("--machine")
        .arg(&machine)
        .arg("--control")
        .arg(dir.control())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn vmlab-hv");

    let status = wait_for_exit(child, Duration::from_secs(60));
    assert!(status.success(), "{status}");
    let log = serial_log_text(&dir);
    // The kernel found both functions behind this VMM's CF8/CFC and ECAM
    // root, sized and assigned their 64-bit BARs from the windows this
    // machine declares, enabled memory decoding, and the virtio-pci driver
    // accepted both functions' capability lists.
    assert!(
        log.contains("virtio-pci 0000:00:01.0: enabling device"),
        "the block function must be assigned and enabled: {log}"
    );
    assert!(
        log.contains("virtio-pci 0000:00:02.0: enabling device"),
        "the console function must be assigned and enabled: {log}"
    );
    assert!(
        !log.contains("probe with driver virtio-pci failed"),
        "virtio-pci must accept both functions: {log}"
    );
}

#[test]
fn bad_machine_file_is_a_clear_launch_error() {
    let dir = Dir::new("badjson");
    let machine = dir.path.join("machine.json");

    // A machine description naming a missing kernel fails with exit 2.
    std::fs::write(
        &machine,
        r#"{"name":"x","arch":"x86_64","accel":"kvm","vcpus":1,"memory_mib":64,
            "boot":{"kind":"kernel","kernel":"/nonexistent/vmlinuz","initrd":null,"cmdline":""},
            "disks":[],"nics":[],"serial_log":"/dev/null","ports":[],"shares":[]}"#,
    )
    .unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_vmlab-hv"))
        .arg("--machine")
        .arg(&machine)
        .arg("--control")
        .arg(dir.control())
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(2));
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("/nonexistent/vmlinuz"),
        "the error must name the kernel: {stderr}"
    );

    // A malformed JSON file fails the same way.
    std::fs::write(&machine, "not json").unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_vmlab-hv"))
        .arg("--machine")
        .arg(&machine)
        .arg("--control")
        .arg(dir.control())
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(2));

    // Usage errors too.
    let out = Command::new(env!("CARGO_BIN_EXE_vmlab-hv"))
        .arg("--machine")
        .arg(&machine)
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(2));
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("--control"), "{stderr}");

    // An existing control socket path is refused, not clobbered.
    let existing = dir.path.join("existing.sock");
    let _ = std::process::Command::new("touch").arg(&existing).status();
    // Any existing file passes the kernel check, which runs before the
    // control socket is bound; the guest never boots.
    let kernel = dir.path.join("vmlinuz");
    std::fs::write(&kernel, b"not a kernel").unwrap();
    std::fs::write(
        &machine,
        format!(
            r#"{{"name":"x","arch":"x86_64","accel":"kvm","vcpus":1,"memory_mib":64,
            "boot":{{"kind":"kernel","kernel":"{}","initrd":null,"cmdline":""}},
            "disks":[],"nics":[],"serial_log":"/dev/null","ports":[],"shares":[]}}"#,
            kernel.display()
        ),
    )
    .unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_vmlab-hv"))
        .arg("--machine")
        .arg(&machine)
        .arg("--control")
        .arg(&existing)
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(2));
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("control socket"), "{stderr}");
    assert!(existing.exists(), "the existing path is not removed");
}
