//! Launch-contract tests: spawn the `vmlab-hv` binary and drive it over the
//! control socket, as vmlab will. These are the S1-adjacent checks this work
//! package owns: the process contract (bind, run, exit codes) and the
//! command surface. The kernel asset is vmlab's own container kernel; the
//! tests skip with a printed reason when it or `/dev/kvm` is missing.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};
use vmlab_hv_tests::process::ManagedChild;

use vmlab_hv_proto::client::Client;
use vmlab_hv_proto::control::{Command as Ctrl, Event, Outcome, RunState, ShutdownReason};

const KERNEL: &str = "/home/wil/dev/vmlab/guest/dist/x86_64/vmlinuz";

fn kernel_available() -> bool {
    Path::new("/dev/kvm").exists() && Path::new(KERNEL).exists()
}

struct Dir {
    path: PathBuf,
    _dir: tempfile::TempDir,
}

impl Dir {
    fn new(tag: &str) -> Dir {
        let dir = tempfile::Builder::new()
            .prefix(&format!("hv-launch-{tag}-"))
            .tempdir()
            .unwrap();
        Dir {
            path: dir.path().to_path_buf(),
            _dir: dir,
        }
    }

    fn machine(&self) -> PathBuf {
        let json = format!(
            r#"{{
                "name": "launch-test",
                "exit_on_reset": true,
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

fn wait_for_exit(mut child: ManagedChild, timeout: Duration) -> std::process::ExitStatus {
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
fn legacy_vga_writes_are_visible_while_the_guest_is_halted() {
    use std::fs::File;
    use std::os::unix::fs::FileExt;
    use vmlab_hv_display::{Capabilities, Client as DisplayClient, Message, SurfaceKind};

    if !Path::new("/dev/kvm").exists() || !Path::new("/usr/share/qemu/vgabios-stdvga.bin").is_file()
    {
        eprintln!("SKIP VGA S1: /dev/kvm or SeaVGABIOS missing");
        return;
    }
    let dir = Dir::new("coalesced-vga");
    let mut bios = vec![0; 0x10000];
    let mut code = vec![0xfa]; // cli
    for (index, value) in [(1u16, 64u16), (2, 64), (3, 32), (4, 0x41)] {
        code.extend_from_slice(&[0xba, 0xce, 0x01, 0xb8]); // mov dx, 0x1ce; mov ax, index
        code.extend_from_slice(&index.to_le_bytes());
        code.extend_from_slice(&[0xef, 0x42, 0xb8]); // out dx, ax; inc dx; mov ax, value
        code.extend_from_slice(&value.to_le_bytes());
        code.push(0xef); // out dx, ax
    }
    code.extend_from_slice(&[0xb8, 0x00, 0xa0, 0x8e, 0xc0]); // mov ax, 0xa000; mov es, ax
    // mov dword es:[0], 0x00112233; hlt; jmp hlt. No I/O follows this write.
    code.extend_from_slice(&[
        0x26, 0x66, 0xc7, 0x06, 0, 0, 0x33, 0x22, 0x11, 0, 0xf4, 0xeb, 0xfd,
    ]);
    bios[..code.len()].copy_from_slice(&code);
    bios[0xfff0..0xfff5].copy_from_slice(&[0xea, 0, 0, 0, 0xf0]);
    let firmware = dir.path.join("bios.bin");
    std::fs::write(&firmware, bios).unwrap();
    let machine = dir.machine();
    let mut description: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&machine).unwrap()).unwrap();
    description["machine"] = "q35".into();
    description["boot"] = serde_json::json!({
        "kind":"firmware", "firmware":"seabios", "code":firmware
    });
    description["display"] = serde_json::json!({
        "device":"std-vga", "pci":{"bus":0,"device":4,"function":0}, "rom":null,
        "socket":dir.path.join("display.sock"), "vnc_socket":dir.path.join("vnc.sock")
    });
    std::fs::write(&machine, serde_json::to_vec(&description).unwrap()).unwrap();
    let log = File::create(dir.path.join("hv.log")).unwrap();
    let mut child = ManagedChild::spawn(
        Command::new(env!("CARGO_BIN_EXE_vmlab-hv"))
            .arg("--machine")
            .arg(machine)
            .arg("--control")
            .arg(dir.control())
            .arg("--paused")
            .stdout(Stdio::null())
            .stderr(Stdio::from(log)),
    )
    .unwrap();
    let deadline = Instant::now() + Duration::from_secs(20);
    let mut control = loop {
        if let Ok(control) = Client::connect(dir.control()) {
            break control;
        }
        assert!(
            Instant::now() < deadline && child.try_wait().unwrap().is_none(),
            "VGA VM failed to start: {}",
            std::fs::read_to_string(dir.path.join("hv.log")).unwrap_or_default()
        );
        std::thread::sleep(Duration::from_millis(10));
    };
    assert!(matches!(
        control.call(Ctrl::Resume).unwrap(),
        Outcome::Ok { .. }
    ));
    let display = DisplayClient::connect(
        dir.path.join("display.sock"),
        Capabilities(Capabilities::FRAMES | Capabilities::CPU_COPY),
    )
    .unwrap();
    display
        .stream()
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    let mut buffers = Vec::new();
    let mut dimensions = (0, 0);
    loop {
        let packet = display.receive().unwrap();
        match packet.message {
            Message::SurfaceConfigure {
                width,
                height,
                kind: SurfaceKind::Shm,
                ..
            } => {
                dimensions = (width, height);
                buffers = packet.fds;
            }
            Message::FrameReady {
                surface_id,
                buffer,
                seq,
                ..
            } => {
                let mut pixel = [0; 4];
                if dimensions == (64, 64) {
                    let file = File::from(buffers[buffer as usize].try_clone().unwrap());
                    file.read_exact_at(&mut pixel, 0).unwrap();
                }
                display.release(surface_id, buffer, seq).unwrap();
                if dimensions == (64, 64) && pixel[..3] == [0x33, 0x22, 0x11] {
                    break;
                }
                assert!(
                    Instant::now() < deadline,
                    "halted guest's VGA write was not published"
                );
            }
            _ => {}
        }
    }
    assert!(matches!(
        control.call(Ctrl::Quit).unwrap(),
        Outcome::Ok { .. }
    ));
    assert!(wait_for_exit(child, Duration::from_secs(10)).success());
}

#[test]
fn malformed_secret_descriptions_do_not_echo_values() {
    let dir = Dir::new("malformed-secret");
    let machine = dir.machine();
    let description: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&machine).unwrap()).unwrap();
    let fake = "FAKE".repeat(16);
    let mut wrong_section = description.clone();
    wrong_section["applesmc"] = fake.clone().into();
    let mut wrong_field = description;
    wrong_field["applesmc"] = serde_json::json!({"osk":fake});
    wrong_field["applesmc"]
        .as_object_mut()
        .unwrap()
        .insert(fake.clone(), 0.into());
    for value in [
        wrong_section,
        wrong_field,
        serde_json::Value::String(fake.clone()),
    ] {
        std::fs::write(&machine, serde_json::to_vec(&value).unwrap()).unwrap();
        let output = Command::new(env!("CARGO_BIN_EXE_vmlab-hv"))
            .arg("--machine")
            .arg(&machine)
            .arg("--control")
            .arg(dir.control())
            .output()
            .unwrap();
        assert_eq!(output.status.code(), Some(2));
        assert!(!String::from_utf8_lossy(&output.stdout).contains(&fake));
        let error = String::from_utf8_lossy(&output.stderr);
        assert!(!error.contains(&fake));
        assert!(error.contains("invalid machine description at line "));
        assert!(error.contains(", column "));
        assert!(!dir.control().exists());
    }
}

#[test]
fn display_startup_failure_preserves_targets_and_never_advertises_control() {
    if !Path::new("/dev/kvm").exists()
        || !Path::new("/usr/share/qemu/bios-256k.bin").exists()
        || !Path::new("/usr/share/qemu/vgabios-stdvga.bin").exists()
    {
        eprintln!("skipping display startup failure: KVM or VGA firmware unavailable");
        return;
    }
    for occupied in [true, false] {
        let _permit = vmlab_hv_tests::process::VmPermit::acquire().unwrap();
        let dir = Dir::new("display-startup");
        let machine = dir.machine();
        let native = dir.path.join("native.sock");
        let rfb = dir.path.join("missing/vnc.sock");
        if occupied {
            std::fs::write(&native, b"existing target").unwrap();
        }
        let mut desc: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&machine).unwrap()).unwrap();
        desc["machine"] = "q35".into();
        desc["boot"] = serde_json::json!({"kind":"firmware", "firmware":"seabios", "code":"/usr/share/qemu/bios-256k.bin"});
        desc["display"] = serde_json::json!({"device":"std-vga", "pci":{"bus":0,"device":4,"function":0}, "rom":null, "socket":native, "vnc_socket":rfb});
        std::fs::write(&machine, serde_json::to_vec(&desc).unwrap()).unwrap();
        let result = Command::new(env!("CARGO_BIN_EXE_vmlab-hv"))
            .arg("--machine")
            .arg(machine)
            .arg("--control")
            .arg(dir.control())
            .arg("--paused")
            .output()
            .unwrap();
        assert_eq!(
            result.status.code(),
            Some(2),
            "{}",
            String::from_utf8_lossy(&result.stderr)
        );
        assert!(
            !dir.control().exists(),
            "control became available before display startup succeeded"
        );
        if occupied {
            assert_eq!(std::fs::read(native).unwrap(), b"existing target");
        } else {
            assert!(
                !native.exists(),
                "failed RFB binding left the native socket behind"
            );
        }
    }
}

#[test]
fn panic_after_control_ready_kills_and_reaps_vm() {
    if !kernel_available() {
        eprintln!("skipping: /dev/kvm or container kernel is absent");
        return;
    }
    let dir = Dir::new("unwind");
    let mut pid = 0;
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let child = ManagedChild::spawn(
            Command::new(env!("CARGO_BIN_EXE_vmlab-hv"))
                .arg("--machine")
                .arg(dir.machine())
                .arg("--control")
                .arg(dir.control())
                .arg("--paused")
                .stdout(Stdio::null())
                .stderr(Stdio::null()),
        )
        .unwrap();
        pid = child.id();
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            if Client::connect(dir.control()).is_ok() {
                break;
            }
            assert!(Instant::now() < deadline, "control startup timeout");
            std::thread::sleep(Duration::from_millis(20));
        }
        panic!("injected failure after VM startup");
    }));
    assert!(result.is_err());
    assert_ne!(pid, 0);
    assert!(
        !Path::new(&format!("/proc/{pid}")).exists(),
        "VM survived panic"
    );
}

#[test]
fn boots_kernel_and_exits_zero_on_guest_reset() {
    if !kernel_available() {
        eprintln!("skipping: /dev/kvm or the container kernel is not present");
        return;
    }
    let dir = Dir::new("reset");
    let machine = dir.machine();

    let child = ManagedChild::spawn(
        Command::new(env!("CARGO_BIN_EXE_vmlab-hv"))
            .arg("--machine")
            .arg(&machine)
            .arg("--control")
            .arg(dir.control())
            .stdout(Stdio::null())
            .stderr(Stdio::null()),
    )
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
fn kernel_reset_reboots_in_the_same_process_and_control_connection() {
    if !kernel_available() {
        eprintln!("skipping: /dev/kvm or the container kernel is not present");
        return;
    }
    for method in ["t", "k", "p", "a"] {
        kernel_reset_with_method(method);
    }
}

fn kernel_reset_with_method(method: &str) {
    let dir = Dir::new("in-place-reset");
    let machine = dir.machine();
    let mut description: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&machine).unwrap()).unwrap();
    description.as_object_mut().unwrap().remove("exit_on_reset");
    description["vcpus"] = 2.into();
    description["boot"]["cmdline"] = format!("console=ttyS0 panic=-1 reboot={method}").into();
    let switch = std::os::unix::net::UnixListener::bind(dir.path.join("switch.sock")).unwrap();
    switch.set_nonblocking(true).unwrap();
    description["nics"] = serde_json::json!([{
        "mac":"52:54:00:00:00:01", "attachment":{"kind":"stream", "socket":dir.path.join("switch.sock")}
    }]);
    std::fs::write(&machine, serde_json::to_vec(&description).unwrap()).unwrap();
    let mut child = ManagedChild::spawn(
        Command::new(env!("CARGO_BIN_EXE_vmlab-hv"))
            .arg("--machine")
            .arg(machine)
            .arg("--control")
            .arg(dir.control())
            .arg("--paused")
            .stdout(Stdio::null())
            .stderr(Stdio::null()),
    )
    .unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut client = loop {
        if let Ok(client) = Client::connect(dir.control()) {
            break client;
        }
        assert!(Instant::now() < deadline, "control startup timeout");
        std::thread::sleep(Duration::from_millis(10));
    };
    client.call(Ctrl::Resume).unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    let (mut network, _) = loop {
        match switch.accept() {
            Ok(connection) => break connection,
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                assert!(Instant::now() < deadline, "NIC connection timeout");
                std::thread::sleep(Duration::from_millis(10));
            }
            Err(error) => panic!("NIC connection: {error}"),
        }
    };
    network
        .set_read_timeout(Some(Duration::from_millis(100)))
        .unwrap();
    for _ in 0..2 {
        loop {
            match client
                .wait_event()
                .unwrap()
                .expect("control remains connected")
            {
                Event::Reset => break,
                Event::Stopped | Event::Resumed => {}
                event => panic!("unexpected event during reset: {event:?}"),
            }
        }
        assert!(
            child.try_wait().unwrap().is_none(),
            "reset ended the process"
        );
        assert!(matches!(
            client.call(Ctrl::Status).unwrap(),
            Outcome::Ok { .. }
        ));
        use std::io::Read;
        match network.read(&mut [0; 4096]) {
            Ok(count) => assert_ne!(count, 0, "reset closed the NIC stream"),
            Err(error) => assert!(
                matches!(
                    error.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ),
                "NIC stream failed after reset: {error}"
            ),
        }
        assert!(
            matches!(switch.accept(), Err(error) if error.kind() == std::io::ErrorKind::WouldBlock),
            "reset reconnected the NIC stream"
        );
    }
    client.call(Ctrl::Quit).unwrap();
    assert!(wait_for_exit(child, Duration::from_secs(10)).success());
    assert!(serial_log_text(&dir).matches("Linux version").count() >= 2);
}

#[test]
fn paused_machine_resumes_over_the_control_socket() {
    if !kernel_available() {
        eprintln!("skipping: /dev/kvm or the container kernel is not present");
        return;
    }
    let dir = Dir::new("paused");
    let machine = dir.machine();

    let mut child = ManagedChild::spawn(
        Command::new(env!("CARGO_BIN_EXE_vmlab-hv"))
            .arg("--machine")
            .arg(&machine)
            .arg("--control")
            .arg(dir.control())
            .arg("--paused")
            .stdout(Stdio::null())
            .stderr(Stdio::null()),
    )
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

    let mut child = ManagedChild::spawn(
        Command::new(env!("CARGO_BIN_EXE_vmlab-hv"))
            .arg("--machine")
            .arg(&machine)
            .arg("--control")
            .arg(dir.control())
            .arg("--paused")
            .stdout(Stdio::null())
            .stderr(Stdio::null()),
    )
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
            "exit_on_reset": true,
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

    let child = ManagedChild::spawn(
        Command::new(env!("CARGO_BIN_EXE_vmlab-hv"))
            .arg("--machine")
            .arg(&machine)
            .arg("--control")
            .arg(dir.control())
            .stdout(Stdio::null())
            .stderr(Stdio::null()),
    )
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
