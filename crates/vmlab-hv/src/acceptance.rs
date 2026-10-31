//! P1 firmware/template acceptance through the executable's control socket.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};
use vmlab_hv_tests::process::ManagedChild;

use vmlab_hv_proto::{
    client::Client,
    control::{Command as Control, Outcome},
};
use vmlab_hv_tests::agent::AgentClient;

struct Process(ManagedChild);

#[derive(Clone, Copy, PartialEq, Eq)]
enum Firmware {
    SeaBios,
    Ovmf,
    SecureOvmf,
}

pub(super) fn template(name: &str, variable: &str) -> Option<PathBuf> {
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
    let disk = versions.pop()?;
    let standard = disk.parent()?.join("variants/standard/disk.qcow2");
    Some(if name == "windows-10" && standard.is_file() {
        standard
    } else {
        disk
    })
}

enum DisplayTest {
    None,
    Stream,
    Reset,
    Screenshot(&'static str),
}

fn boot_template(
    name: &str,
    variable: &str,
    snapshot: bool,
    bus: vmlab_hv_proto::machine::DiskBus,
    display_test: DisplayTest,
    firmware: Firmware,
) {
    if !Path::new("/dev/kvm").exists() {
        eprintln!("skipping {name}: /dev/kvm is unavailable");
        return;
    }
    if !matches!(display_test, DisplayTest::None)
        && !Command::new("tesseract")
            .arg("--version")
            .output()
            .is_ok_and(|o| o.status.success())
    {
        eprintln!("skipping P3: tesseract is unavailable for firmware/console frame recognition");
        return;
    }
    if (firmware == Firmware::SeaBios && !Path::new("/usr/share/qemu/bios-256k.bin").is_file())
        || !Path::new("/usr/share/qemu/vgabios-virtio.bin").is_file()
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
    let firmware_root = std::env::var_os("VMLAB_HV_FIRMWARE_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| "/home/wil/dev/vmlab/guest/dist/firmware/x86_64".into());
    let secure = firmware == Firmware::SecureOvmf;
    let rom = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target/ipxe/virtio-net.rom");
    if firmware != Firmware::SeaBios {
        if !rom.is_file() {
            eprintln!("skipping {name}: run just firmware-ipxe-build for the OVMF NIC ROM");
            return;
        }
        for file in if secure {
            ["OVMF_CODE_4M.secboot.fd", "OVMF_VARS_4M.ms.fd"]
        } else {
            ["OVMF_CODE_4M.fd", "OVMF_VARS_4M.fd"]
        } {
            if !firmware_root.join(file).is_file() {
                eprintln!("skipping {name}: {file} is absent; set VMLAB_HV_FIRMWARE_DIR");
                return;
            }
        }
    }
    let windows = name.starts_with("windows-");
    let tpm = name == "windows-server-2022";
    if tpm
        && !Command::new("swtpm")
            .arg("--version")
            .output()
            .is_ok_and(|output| output.status.success())
    {
        eprintln!("skipping {name}: swtpm is unavailable");
        return;
    }
    let mut dir = tempfile::Builder::new().prefix("hv-p1-").tempdir().unwrap();
    if std::env::var_os("VMLAB_HV_ACCEPTANCE_KEEP").is_some() {
        dir.disable_cleanup(true);
        eprintln!("{name} acceptance artifacts: {}", dir.path().display());
    }
    let spawn_tpm = || {
        let socket = dir.path().join("tpm.sock");
        let state = dir.path().join("tpm");
        std::fs::create_dir_all(&state).unwrap();
        let mut child = Process(
            ManagedChild::helper(
                Command::new("swtpm")
                    .args(["socket", "--tpm2", "--tpmstate"])
                    .arg(format!("dir={}", state.display()))
                    .arg("--ctrl")
                    .arg(format!("type=unixio,path={}", socket.display()))
                    .stdout(Stdio::null()),
            )
            .unwrap(),
        );
        let deadline = Instant::now() + Duration::from_secs(5);
        while !socket.exists() {
            assert!(
                child.0.try_wait().unwrap().is_none(),
                "swtpm exited before binding"
            );
            assert!(Instant::now() < deadline, "swtpm did not bind");
            std::thread::sleep(Duration::from_millis(10));
        }
        child
    };
    let _swtpm = tpm.then(spawn_tpm);
    let disk = dir.path().join("disk.qcow2");
    vmlab_disk::ops::create_linked_clone(&template, &disk).unwrap();
    let control = dir.path().join("control.sock");
    let agent = dir.path().join("agent.sock");
    let serial = dir.path().join("serial.log");
    let stderr = dir.path().join("stderr.log");
    let native = dir.path().join("native-custom.sock");
    let rfb = dir.path().join("rfb-custom.sock");
    let mut description = serde_json::json!({
        "machine": "q35", "name": name, "arch": "x86_64", "accel": "kvm", "vcpus": 2, "memory_mib": 4096,
        "boot": {"kind":"firmware", "firmware":"seabios", "code":"/usr/share/qemu/bios-256k.bin"},
        "disks":[{"id":"disk0", "pci":{"bus":0,"device":2,"function":0}, "path":disk,"format":"qcow2","read_only":false,"vmstate":true}],
        "console_pci":{"bus":0,"device":1,"function":0},
        "ahci_pci":{"bus":0,"device":31,"function":2},
        "display":{"device":"virtio-vga","pci":{"bus":0,"device":4,"function":0},"rom":null},
        "tablet":{"bus":0,"device":5,"function":0},
        "usb":{"bus":0,"device":5,"function":0},
        "ports":[{"name":"vmlab.agent.0", "socket":agent}], "nics":[],"serial_log":serial
    });
    if windows || secure {
        description["vcpus"] = 4.into();
        description["memory_mib"] = 8192.into();
        description["hyperv"] = serde_json::json!([
            "relaxed",
            "vapic",
            "spinlocks",
            "vpindex",
            "runtime",
            "time",
            "synic",
            "stimer",
            "frequencies",
            "reset",
            "tlbflush",
            "ipi"
        ]);
        if vmlab_hv_vmm::hyperv::host_apic_virtualization() {
            description["hyperv"]
                .as_array_mut()
                .unwrap()
                .push("avic".into());
        }
    }
    if tpm {
        description["tpm"] = serde_json::json!({"socket":dir.path().join("tpm.sock")});
    }
    if firmware != Firmware::SeaBios {
        let root = firmware_root;
        description["nics"] = serde_json::json!([{
            "pci":{"bus":0,"device":3,"function":0}, "mac":"52:54:00:00:00:01",
            "attachment":{"kind":"stream","socket":dir.path().join("switch.sock")},
            "rom":rom
        }]);
        let vars = dir.path().join("OVMF_VARS.fd");
        let template_vars = template
            .ancestors()
            .skip(1)
            .find(|path| path.join("template.wcl").is_file())
            .unwrap_or_else(|| template.parent().unwrap())
            .join("OVMF_VARS.fd");
        std::fs::copy(
            if template_vars.is_file() {
                template_vars
            } else {
                root.join(if secure {
                    "OVMF_VARS_4M.ms.fd"
                } else {
                    "OVMF_VARS_4M.fd"
                })
            },
            &vars,
        )
        .unwrap();
        description["boot"] = serde_json::json!({
            "kind":"firmware", "firmware":"ovmf", "code":root.join(if secure {"OVMF_CODE_4M.secboot.fd"} else {"OVMF_CODE_4M.fd"}), "vars":vars, "secure_boot":secure
        });
        if name == "debian-13" {
            description["memory_mib"] = 2048.into();
        }
    }
    if let DisplayTest::Screenshot(device) = display_test {
        description["display"]["device"] = device.into();
    }
    if !matches!(display_test, DisplayTest::None) {
        description["display"]["socket"] = serde_json::to_value(&native).unwrap();
        description["display"]["vnc_socket"] = serde_json::to_value(&rfb).unwrap();
    }
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
    let spawn = || {
        Process(
            ManagedChild::spawn(
                Command::new(&binary)
                    .arg("--machine")
                    .arg(&machine)
                    .arg("--control")
                    .arg(&control)
                    .arg("--paused")
                    .env("VMLAB_HV_DIAGNOSTICS", "1")
                    .stdout(Stdio::null())
                    .stderr(
                        std::fs::OpenOptions::new()
                            .create(true)
                            .append(true)
                            .open(&stderr)
                            .unwrap(),
                    ),
            )
            .unwrap(),
        )
    };
    let mut process = spawn();
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
    let display = matches!(display_test, DisplayTest::Stream)
        .then(|| crate::display_acceptance::Observer::connect(&native));
    if !matches!(display_test, DisplayTest::None) {
        assert!(!dir.path().join("display.sock").exists());
        assert!(!dir.path().join("vnc.sock").exists());
    }
    assert!(matches!(
        client.call(Control::Resume).unwrap(),
        Outcome::Ok { .. }
    ));
    let timeout = std::env::var("VMLAB_HV_TEMPLATE_TIMEOUT")
        .ok()
        .map(|v| v.parse().unwrap())
        .unwrap_or(if windows { 600 } else { 120 });
    let deadline = Instant::now() + Duration::from_secs(timeout);
    let mut windows_setup = windows;
    let mut requested_reboot = false;
    let mut setup_status = String::new();
    let mut guest = loop {
        if !requested_reboot
            && let Ok(mut guest) = AgentClient::connect(&agent, Duration::from_secs(3))
        {
            if !windows_setup {
                break guest;
            }
            match guest.exec(
                &["powershell.exe", "-NoProfile", "-NonInteractive", "-Command",
                    "$s = Get-ItemProperty -LiteralPath 'HKLM:\\SYSTEM\\Setup' -ErrorAction Stop; \
                     Write-Output ('setup=' + $s.SystemSetupInProgress + ', oobe=' + $s.OOBEInProgress); \
                     if ($s.SystemSetupInProgress -eq 0 -and $s.OOBEInProgress -eq 0) { exit 0 }; exit 1"],
                Duration::from_secs(20),
            ) {
                Ok(output) if output.exit_code == 0 => {
                    let reboot = guest.exec(&["shutdown.exe", "/r", "/t", "5", "/f"], Duration::from_secs(20))
                        .unwrap_or_else(|error| panic!("{name} first-boot reboot: {error}; {}", diagnostics()));
                    assert_eq!(reboot.exit_code, 0, "{name} first-boot reboot: {reboot:?}");
                    eprintln!("{name}: setup finished; rebooting before acceptance");
                    requested_reboot = true;
                }
                Ok(output) => setup_status = format!("exit {}: {}{}", output.exit_code,
                    String::from_utf8_lossy(&output.stdout), String::from_utf8_lossy(&output.stderr)),
                Err(error) => setup_status = error,
            }
        }
        assert!(
            process.0.try_wait().unwrap().is_none(),
            "{name} exited before agent ready: {}",
            diagnostics()
        );
        client.call(Control::Status).unwrap();
        for event in client.take_events() {
            if event == vmlab_hv_proto::control::Event::Reset && requested_reboot {
                requested_reboot = false;
                windows_setup = false;
            }
        }
        if Instant::now() >= deadline {
            let _ = client.call(Control::Quit);
            let _ = process.0.wait();
            panic!(
                "{name} agent/setup timeout ({setup_status}): {}",
                diagnostics()
            );
        }
        if windows_setup || requested_reboot {
            std::thread::sleep(Duration::from_millis(100));
        }
    };
    let shell = if windows { "cmd.exe" } else { "/bin/sh" };
    let flag = if windows { "/C" } else { "-c" };
    let result = guest
        .exec(
            &[
                shell,
                flag,
                if windows {
                    "echo firmware-agent-ready"
                } else {
                    "printf firmware-agent-ready"
                },
            ],
            Duration::from_secs(20),
        )
        .unwrap();
    assert_eq!(result.exit_code, 0);
    assert_eq!(
        String::from_utf8_lossy(&result.stdout).trim(),
        "firmware-agent-ready"
    );
    if matches!(display_test, DisplayTest::Reset) {
        let observer = crate::display_acceptance::Observer::connect(&native);
        let pid = process.0.id();
        let reboot = if windows {
            vec!["shutdown.exe", "/r", "/t", "2", "/f"]
        } else {
            vec![
                "sh",
                "-c",
                "nohup sh -c 'sleep 2; reboot' >/dev/null 2>&1 </dev/null &",
            ]
        };
        assert_eq!(
            guest
                .exec(&reboot, Duration::from_secs(20))
                .unwrap()
                .exit_code,
            0
        );
        drop(guest);
        loop {
            match client
                .wait_event()
                .unwrap()
                .expect("control must survive guest reset")
            {
                vmlab_hv_proto::control::Event::Reset => break,
                vmlab_hv_proto::control::Event::Shutdown { reason } => {
                    panic!("reset shut down: {reason:?}")
                }
                _ => {}
            }
        }
        let before = observer.frame_count();
        assert!(
            process.0.try_wait().unwrap().is_none(),
            "guest reset ended process: {}",
            diagnostics()
        );
        assert_eq!(process.0.id(), pid);
        guest = AgentClient::connect(&agent, Duration::from_secs(timeout))
            .unwrap_or_else(|error| panic!("{name} agent after reset: {error}; {}", diagnostics()));
        let output = guest
            .exec(&[shell, flag, "echo rebooted"], Duration::from_secs(20))
            .unwrap();
        assert_eq!(output.exit_code, 0);
        assert!(String::from_utf8_lossy(&output.stdout).contains("rebooted"));
        observer.wait_after_reset(before);
        assert!(matches!(
            client.call(Control::Status).unwrap(),
            Outcome::Ok { .. }
        ));
        eprintln!(
            "{name}: in-place reset retained PID {pid}, control and display; agent re-handshook"
        );
    }
    if firmware == Firmware::SecureOvmf {
        let secure = guest.exec(&["/bin/sh", "-c", "od -An -tu1 /sys/firmware/efi/efivars/SecureBoot-8be4df61-93ca-11d2-aa0d-00e098032b8c"], Duration::from_secs(20)).unwrap();
        assert_eq!(secure.exit_code, 0, "{:?}", secure);
        assert_eq!(
            secure
                .stdout
                .split(|c| c.is_ascii_whitespace())
                .rfind(|s| !s.is_empty()),
            Some(&b"1"[..]),
            "{:?}",
            secure
        );
        let keys = guest
            .exec(
                &[
                    "/bin/sh",
                    "-c",
                    "cd /sys/firmware/efi/efivars && \
                     test \"$(od -An -tu1 -j4 -N1 SetupMode-8be4df61-93ca-11d2-aa0d-00e098032b8c)\" -eq 0 && \
                     test -s PK-8be4df61-93ca-11d2-aa0d-00e098032b8c && \
                     test -s KEK-8be4df61-93ca-11d2-aa0d-00e098032b8c && \
                     test -s db-d719b2cb-3d3a-4596-a3bc-dad00e67656f",
                ],
                Duration::from_secs(20),
            )
            .unwrap();
        assert_eq!(keys.exit_code, 0, "enrolled secure-boot keys: {keys:?}");
        let enumeration = guest
            .exec(
                &[
                    "/bin/sh",
                    "-ec",
                    "for attempt in $(seq 1 20); do \
                     umount /sys/firmware/efi/efivars; \
                     mount -t efivarfs efivarfs /sys/firmware/efi/efivars; \
                     if ! test -s /sys/firmware/efi/efivars/SecureBoot-8be4df61-93ca-11d2-aa0d-00e098032b8c || \
                     ! test -s /sys/firmware/efi/efivars/BootOrder-8be4df61-93ca-11d2-aa0d-00e098032b8c; then \
                     echo EFI enumeration failed at attempt $attempt; \
                     ls /sys/firmware/efi/efivars; dmesg | tail -60; exit 1; fi; \
                     done",
                ],
                Duration::from_secs(120),
            )
            .unwrap();
        assert_eq!(enumeration.exit_code, 0, "EFI enumeration: {enumeration:?}");
    }
    if matches!(display_test, DisplayTest::Screenshot(_)) {
        crate::display_acceptance::screenshots_after_unobserved_boot(&native, &mut guest);
    }
    if let Some(display) = &display {
        display.exercise(&mut guest, &rfb);
    }
    if snapshot {
        guest
            .exec(
                &[
                    shell,
                    flag,
                    if windows {
                        "echo before>C:\\Windows\\Temp\\vmlab-hv-snapshot"
                    } else {
                        "printf before >/run/vmlab-hv-snapshot"
                    },
                ],
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
                &[
                    shell,
                    flag,
                    if windows {
                        "echo after>C:\\Windows\\Temp\\vmlab-hv-snapshot"
                    } else {
                        "printf after >/run/vmlab-hv-snapshot"
                    },
                ],
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
            .exec(
                &[
                    shell,
                    flag,
                    if windows {
                        "type C:\\Windows\\Temp\\vmlab-hv-snapshot"
                    } else {
                        "cat /run/vmlab-hv-snapshot"
                    },
                ],
                Duration::from_secs(20),
            )
            .unwrap_or_else(|error| {
                let _ = client.call(Control::Quit);
                let _ = process.0.wait();
                panic!(
                    "{name} exec after snapshot load: {error}; {}",
                    diagnostics()
                );
            });
        assert_eq!(String::from_utf8_lossy(&restored.stdout).trim(), "before");
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
        DisplayTest::None,
        Firmware::SeaBios,
    );
}

#[test]
fn almalinux_10_firmware_agent() {
    boot_template(
        "almalinux-10",
        "VMLAB_HV_TEMPLATE_ALMALINUX_10",
        false,
        vmlab_hv_proto::machine::DiskBus::Virtio,
        DisplayTest::None,
        Firmware::SeaBios,
    );
}

#[test]
fn ubuntu_26_04_ahci_firmware_agent() {
    boot_template(
        "ubuntu-26.04",
        "VMLAB_HV_TEMPLATE_UBUNTU_26_04",
        false,
        vmlab_hv_proto::machine::DiskBus::Ahci,
        DisplayTest::None,
        Firmware::SeaBios,
    );
}

#[test]
fn ubuntu_26_04_native_display_input_and_rfb() {
    boot_template(
        "ubuntu-26.04",
        "VMLAB_HV_TEMPLATE_UBUNTU_26_04",
        false,
        vmlab_hv_proto::machine::DiskBus::Virtio,
        DisplayTest::Stream,
        Firmware::SeaBios,
    );
}

#[test]
fn debian_13_ovmf_agent() {
    boot_template(
        "debian-13",
        "VMLAB_HV_TEMPLATE_DEBIAN_13",
        false,
        vmlab_hv_proto::machine::DiskBus::Virtio,
        DisplayTest::None,
        Firmware::Ovmf,
    );
}

#[test]
fn ubuntu_24_04_secure_ovmf_agent() {
    boot_template(
        "ubuntu-24.04",
        "VMLAB_HV_TEMPLATE_UBUNTU_24_04",
        false,
        vmlab_hv_proto::machine::DiskBus::Virtio,
        DisplayTest::None,
        Firmware::SecureOvmf,
    );
}

#[test]
fn ubuntu_24_04_ovmf_agent() {
    boot_template(
        "ubuntu-24.04",
        "VMLAB_HV_TEMPLATE_UBUNTU_24_04",
        false,
        vmlab_hv_proto::machine::DiskBus::Virtio,
        DisplayTest::None,
        Firmware::Ovmf,
    );
}

#[test]
fn windows_10_ovmf_agent_and_snapshot() {
    boot_template(
        "windows-10",
        "VMLAB_HV_TEMPLATE_WINDOWS_10",
        true,
        vmlab_hv_proto::machine::DiskBus::Virtio,
        DisplayTest::None,
        Firmware::Ovmf,
    );
}

#[test]
fn ubuntu_26_04_agent_reboot_keeps_process_control_and_display() {
    boot_template(
        "ubuntu-26.04",
        "VMLAB_HV_TEMPLATE_UBUNTU_26_04",
        false,
        vmlab_hv_proto::machine::DiskBus::Virtio,
        DisplayTest::Reset,
        Firmware::SeaBios,
    );
}

#[test]
fn windows_10_agent_reboot_keeps_process_control_and_display() {
    boot_template(
        "windows-10",
        "VMLAB_HV_TEMPLATE_WINDOWS_10",
        false,
        vmlab_hv_proto::machine::DiskBus::Virtio,
        DisplayTest::Reset,
        Firmware::Ovmf,
    );
}

#[test]
fn windows_server_2022_ovmf_tpm_agent() {
    boot_template(
        "windows-server-2022",
        "VMLAB_HV_TEMPLATE_WINDOWS_SERVER_2022",
        false,
        vmlab_hv_proto::machine::DiskBus::Virtio,
        DisplayTest::None,
        Firmware::Ovmf,
    );
}

#[test]
fn ubuntu_26_04_first_screenshot_after_unobserved_boot() {
    for device in ["std-vga", "virtio-vga"] {
        boot_template(
            "ubuntu-26.04",
            "VMLAB_HV_TEMPLATE_UBUNTU_26_04",
            false,
            vmlab_hv_proto::machine::DiskBus::Virtio,
            DisplayTest::Screenshot(device),
            Firmware::SeaBios,
        );
    }
}

/// Host facts about the one device `VMLAB_HV_TEST_USB_DEVICE` names.
struct UsbDeviceFacts {
    bus: u32,
    devnum: u32,
    port_path: String,
    vendor: String,
    product: String,
    /// The host drivers bound per interface before the attach, as symlink
    /// targets; the detach must put them back.
    drivers: Vec<String>,
}

/// Resolve `VMLAB_HV_TEST_USB_DEVICE` (`<bus>-<port path>`, e.g. `3-2.4`)
/// against sysfs. Never picks a device by itself (host-hardware safety).
fn usb_test_device() -> Option<UsbDeviceFacts> {
    let selector = std::env::var("VMLAB_HV_TEST_USB_DEVICE").ok()?;
    let root = Path::new("/sys/bus/usb/devices").join(&selector);
    if !root.is_dir() {
        eprintln!(
            "skipping: VMLAB_HV_TEST_USB_DEVICE={selector} names no device in sysfs \
             (expected <bus>-<port path>, e.g. 3-2.4)"
        );
        return None;
    }
    let read = |name: &str| {
        std::fs::read_to_string(root.join(name))
            .unwrap_or_default()
            .trim()
            .to_owned()
    };
    let bus: u32 = read("busnum").parse().ok()?;
    let devnum: u32 = read("devnum").parse().ok()?;
    // The port path below the bus: `1-2.4` -> `2.4`, as usb-host's
    // hostport carries it.
    let port_path = selector
        .split_once('-')
        .map(|(_, rest)| rest.to_owned())
        .unwrap_or_default();
    let mut drivers = Vec::new();
    if let Ok(entries) = std::fs::read_dir(&root) {
        for entry in entries.flatten() {
            let name = entry.file_name();
            let Some(name) = name.to_str() else { continue };
            if !name.contains(':') {
                continue; // interface directories carry a colon
            }
            match std::fs::read_link(entry.path().join("driver")) {
                Ok(driver) => drivers.push(driver.to_string_lossy().into_owned()),
                Err(_) => drivers.push(String::new()),
            }
        }
    }
    drivers.sort();
    Some(UsbDeviceFacts {
        bus,
        devnum,
        port_path,
        vendor: read("idVendor"),
        product: read("idProduct"),
        drivers,
    })
}

/// S1: the host device `VMLAB_HV_TEST_USB_DEVICE` names is passed through
/// to the ubuntu-26.04 template, shows up inside the guest, detaches with
/// a `device_deleted` event, and the host kernel driver rebinds.
#[test]
fn usb_host_passthrough_real_device() {
    if !Path::new("/dev/kvm").exists() {
        eprintln!("skipping: /dev/kvm is unavailable");
        return;
    }
    if !Path::new("/usr/share/qemu/bios-256k.bin").is_file()
        || !Path::new("/usr/share/qemu/vgabios-virtio.bin").is_file()
    {
        eprintln!("skipping: SeaBIOS or SeaVGABIOS is not installed");
        return;
    }
    let Some(facts) = usb_test_device() else {
        eprintln!(
            "skipping: set VMLAB_HV_TEST_USB_DEVICE=<bus>-<port path> to a device the owner \
             designated for testing; nothing is picked automatically"
        );
        return;
    };
    let Some(template) = template("ubuntu-26.04", "VMLAB_HV_TEST_TEMPLATE") else {
        eprintln!("skipping: ubuntu-26.04 template is absent");
        return;
    };
    let mut dir = tempfile::Builder::new()
        .prefix("hv-usb-")
        .tempdir()
        .unwrap();
    if std::env::var_os("VMLAB_HV_ACCEPTANCE_KEEP").is_some() {
        dir.disable_cleanup(true);
        eprintln!("usb acceptance artifacts: {}", dir.path().display());
    }
    let disk = dir.path().join("disk.qcow2");
    vmlab_disk::ops::create_linked_clone(&template, &disk).unwrap();
    let control = dir.path().join("control.sock");
    let agent = dir.path().join("agent.sock");
    let serial = dir.path().join("serial.log");
    let stderr = dir.path().join("stderr.log");
    let description = serde_json::json!({
        "machine": "q35", "name": "usb", "arch": "x86_64", "accel": "kvm",
        "vcpus": 2, "memory_mib": 4096,
        "boot": {"kind":"firmware","firmware":"seabios","code":"/usr/share/qemu/bios-256k.bin"},
        "disks":[{"id":"disk0", "pci":{"bus":0,"device":2,"function":0},
                   "path":disk, "format":"qcow2","read_only":false,"vmstate":true}],
        "console_pci":{"bus":0,"device":1,"function":0},
        "ahci_pci":{"bus":0,"device":31,"function":2},
        "display":{"device":"std-vga","pci":{"bus":0,"device":4,"function":0},"rom":null},
        "tablet":{"bus":0,"device":5,"function":0},
        "usb":{"bus":0,"device":5,"function":0},
        "ports":[{"name":"vmlab.agent.0", "socket":agent}], "nics":[], "serial_log":serial
    });
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
        ManagedChild::spawn(
            Command::new(&binary)
                .arg("--machine")
                .arg(&machine)
                .arg("--control")
                .arg(&control)
                .arg("--paused")
                .stdout(Stdio::null())
                .stderr(
                    std::fs::OpenOptions::new()
                        .create(true)
                        .append(true)
                        .open(&stderr)
                        .unwrap(),
                ),
        )
        .unwrap(),
    );
    let diagnostics = || {
        format!(
            "stderr: {}\nserial: {}",
            std::fs::read_to_string(&stderr).unwrap_or_default(),
            std::fs::read_to_string(&serial).unwrap_or_default(),
        )
    };
    let deadline = Instant::now() + Duration::from_secs(20);
    let mut client = loop {
        if let Ok(client) = Client::connect(&control) {
            break client;
        }
        assert!(
            process.0.try_wait().unwrap().is_none(),
            "usb: exited before control ready: {}",
            diagnostics()
        );
        assert!(Instant::now() < deadline, "usb: control timeout");
        std::thread::sleep(Duration::from_millis(20));
    };
    assert!(
        client.has_capability(vmlab_hv_proto::control::Capability::Hotplug),
        "the hotplug capability must be advertised with usb = true"
    );
    assert!(matches!(
        client.call(Control::Resume).unwrap(),
        Outcome::Ok { .. }
    ));
    let deadline = Instant::now()
        + Duration::from_secs(
            std::env::var("VMLAB_HV_TEMPLATE_TIMEOUT")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(180),
        );
    let mut guest = loop {
        if let Ok(guest) = AgentClient::connect(&agent, Duration::from_secs(3)) {
            break guest;
        }
        assert!(
            process.0.try_wait().unwrap().is_none() && Instant::now() < deadline,
            "usb: agent timeout: {}",
            diagnostics()
        );
        std::thread::sleep(Duration::from_millis(100));
    };
    // Attach the designated host device.
    let id = format!("usbhost-{}-{}", facts.bus, facts.devnum);
    let added = client
        .call(Control::DeviceAdd {
            id: id.clone(),
            driver: "usb-host".into(),
            bus: Some("usbx.0".into()),
            hostbus: facts.bus,
            hostaddr: facts.devnum,
            hostport: facts.port_path.clone(),
        })
        .unwrap();
    assert!(matches!(added, Outcome::Ok { .. }), "device_add: {added:?}");
    // The guest enumerates it: lsusb, or the sysfs walk when lsusb is
    // absent from the template.
    let expected = format!("{}:{}", facts.vendor, facts.product).to_lowercase();
    let seen = loop {
        let output = guest
            .exec(
                &[
                    "/bin/sh",
                    "-c",
                    "if command -v lsusb >/dev/null 2>&1; then lsusb; \
                     else for d in /sys/bus/usb/devices/*; do \
                     [ -f \"$d/idVendor\" ] && \
                     echo \"$(cat \"$d/idVendor\"):$(cat \"$d/idProduct\")\"; \
                     done; fi",
                ],
                Duration::from_secs(20),
            )
            .unwrap();
        assert_eq!(output.exit_code, 0, "{output:?}");
        let stdout = String::from_utf8_lossy(&output.stdout).to_lowercase();
        if stdout.contains(&expected) {
            break stdout;
        }
        assert!(
            Instant::now() < deadline,
            "usb: the guest never saw {expected}: {stdout}{}",
            diagnostics()
        );
        std::thread::sleep(Duration::from_millis(500));
    };
    eprintln!("usb: the guest reports the device: {}", seen.trim());
    // Detach: the reply is ok and a device_deleted event follows.
    assert!(matches!(
        client.call(Control::DeviceDel { id: id.clone() }).unwrap(),
        Outcome::Ok { .. }
    ));
    let mut deleted = false;
    let event_deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < event_deadline {
        match client.wait_event().unwrap() {
            Some(vmlab_hv_proto::control::Event::DeviceDeleted { id: gone }) if gone == id => {
                deleted = true;
                break;
            }
            Some(_) => continue,
            None => break,
        }
    }
    assert!(deleted, "usb: no device_deleted event for {id}");
    // The host kernel driver rebound: the interface drivers are back to
    // what they were before the attach.
    let rebound = {
        let root =
            Path::new("/sys/bus/usb/devices").join(format!("{}-{}", facts.bus, facts.port_path));
        let mut drivers = Vec::new();
        if let Ok(entries) = std::fs::read_dir(&root) {
            for entry in entries.flatten() {
                let file_name = entry.file_name();
                let Some(name) = file_name.to_str() else {
                    continue;
                };
                if !name.contains(':') {
                    continue;
                }
                match std::fs::read_link(entry.path().join("driver")) {
                    Ok(driver) => drivers.push(driver.to_string_lossy().into_owned()),
                    Err(_) => drivers.push(String::new()),
                }
            }
        }
        drivers.sort();
        drivers
    };
    assert!(
        !facts.drivers.is_empty() || rebound.is_empty(),
        "driver sets changed meaning: before {:?} after {:?}",
        facts.drivers,
        rebound
    );
    // Every interface that had a driver before has the same driver again.
    for (before, after) in facts.drivers.iter().zip(&rebound) {
        assert_eq!(
            before, after,
            "the host driver did not rebind: before {before:?}, after {after:?}"
        );
    }
    assert!(matches!(
        client.call(Control::Quit).unwrap(),
        Outcome::Ok { .. }
    ));
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if let Some(status) = process.0.try_wait().unwrap() {
            assert!(status.success(), "usb: exit: {}", diagnostics());
            break;
        }
        assert!(Instant::now() < deadline, "usb: did not exit after quit");
        std::thread::sleep(Duration::from_millis(20));
    }
}
