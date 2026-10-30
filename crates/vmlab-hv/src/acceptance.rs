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
    let mut swtpm = tpm.then(spawn_tpm);
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
    let mut reboots = 0;
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
        if let Some(status) = process.0.try_wait().unwrap() {
            let mut reset = false;
            while let Ok(Some(event)) = client.wait_event() {
                reset |= event
                    == vmlab_hv_proto::control::Event::Shutdown {
                        reason: vmlab_hv_proto::control::ShutdownReason::GuestReset,
                    };
            }
            assert!(
                windows && status.success() && reset && reboots < 3,
                "{name} exited before agent ready: {}",
                diagnostics()
            );
            reboots += 1;
            eprintln!("{name}: restarting after guest reset {reboots}");
            if requested_reboot {
                requested_reboot = false;
                windows_setup = false;
            }
            drop(process);
            if let Some(previous) = swtpm.take() {
                drop(previous);
                let socket = dir.path().join("tpm.sock");
                if socket.exists() {
                    std::fs::remove_file(socket).unwrap();
                }
                swtpm = Some(spawn_tpm());
            }
            for socket in [
                &control,
                &agent,
                &native,
                &rfb,
                &dir.path().join("display.sock"),
                &dir.path().join("vnc.sock"),
            ] {
                if socket.exists() {
                    std::fs::remove_file(socket).unwrap();
                }
            }
            process = spawn();
            let ready = Instant::now() + Duration::from_secs(20);
            client = loop {
                if let Ok(client) = Client::connect(&control) {
                    break client;
                }
                assert!(
                    process.0.try_wait().unwrap().is_none() && Instant::now() < ready,
                    "{name} restart failed: {}",
                    diagnostics()
                );
                std::thread::sleep(Duration::from_millis(20));
            };
            assert!(matches!(
                client.call(Control::Resume).unwrap(),
                Outcome::Ok { .. }
            ));
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
            .unwrap();
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
