//! Phase-4 P1 template acceptance through control and the guest's agent socket.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use vmlab_hv_proto::client::Client;
use vmlab_hv_proto::control::{Command as Control, Event, Outcome, ShutdownReason};
use vmlab_hv_tests::agent::{AgentClient, ExecOutput};
use vmlab_hv_tests::process::ManagedChild;

const DOS_BOOT_GUARD_COMMANDS: [&str; 4] = [
    "copy /Y C:\\FDAUTO.BAT C:\\HVAUTO.BAK",
    "echo @del C:\\HVPC.TXT>C:\\HVAUTO.TMP",
    "type C:\\FDAUTO.BAT>>C:\\HVAUTO.TMP",
    "copy /Y C:\\HVAUTO.TMP C:\\FDAUTO.BAT",
];
const DOS_RESTORE_STARTUP: &str = "copy /Y C:\\HVAUTO.BAK C:\\FDAUTO.BAT";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Guest {
    Temple,
    Dos,
    Windows7,
    Windows98,
    Vista,
}

impl Guest {
    fn template(self) -> (&'static str, &'static str, &'static str, &'static str) {
        match self {
            Self::Temple => ("x86_64", "templeos", "5.03.2", "VMLAB_HV_TEMPLATE_TEMPLEOS"),
            Self::Dos => (
                "x86",
                "freedos-1.3",
                "1.3.0",
                "VMLAB_HV_TEMPLATE_FREEDOS_1_3",
            ),
            Self::Windows7 => (
                "x86_64",
                "windows-7",
                "7600.0.1",
                "VMLAB_HV_TEMPLATE_WINDOWS_7",
            ),
            Self::Windows98 => (
                "x86",
                "windows-98-se",
                "4.10.2222.5",
                "VMLAB_HV_TEMPLATE_WINDOWS_98_SE",
            ),
            Self::Vista => (
                "x86_64",
                "windows-vista",
                "6002.0.0",
                "VMLAB_HV_TEMPLATE_WINDOWS_VISTA",
            ),
        }
    }

    fn windows(self) -> bool {
        matches!(self, Self::Windows7 | Self::Vista)
    }

    fn timeout(self) -> Duration {
        Duration::from_secs(
            std::env::var("VMLAB_HV_PC_TIMEOUT")
                .map(|value| value.parse().expect("VMLAB_HV_PC_TIMEOUT must be seconds"))
                .unwrap_or(if self.windows() || self == Self::Windows98 {
                    600
                } else {
                    180
                }),
        )
    }

    fn description(self, directory: &Path, bios: &Path, vga: &Path) -> serde_json::Value {
        let (_, name, _, _) = self.template();
        let mut machine = serde_json::json!({
            "machine":"pc", "name":name, "arch":"x86_64", "accel":"kvm",
            "vcpus":if matches!(self,Self::Dos|Self::Windows98) {1} else {2},
            "memory_mib":match self {Self::Dos => 64, Self::Temple => 512, Self::Windows98=>128, _ => 4096},
            "boot":{"kind":"firmware", "firmware":"seabios", "code":bios},
            "disks":[{"id":"disk0", "bus":"ide", "channel":0, "unit":0,
                      "path":directory.join("disk.qcow2"), "format":"qcow2",
                      "read_only":false, "vmstate":true}],
            "display":{"device":"std-vga", "pci":{"bus":0,"device":2,"function":0}, "rom":vga},
            "ports":[{"name":"vmlab.agent.0", "socket":directory.join("agent.sock"),
                      "transport":if self.windows() {"virtio"} else {"com1"}}],
            "nics":[], "serial_log":directory.join("serial.log"),
            "serial_log_port":if self.windows() {"com1"} else {"com2"}
        });
        if self.windows() {
            machine["console_pci"] = serde_json::json!({"bus":0,"device":3,"function":0});
            machine["hyperv"] = serde_json::json!([
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
            if vmlab_hv_vmm::hyperv::host_apic_virtualization()
                && std::env::var_os("VMLAB_HV_TEST_NO_AVIC").is_none()
            {
                machine["hyperv"]
                    .as_array_mut()
                    .unwrap()
                    .push("avic".into());
            }
        }
        if matches!(self, Self::Dos | Self::Windows98) {
            machine["rtc_base"] = "localtime".into();
        }
        if self == Self::Windows98 {
            machine["cpu_model"] = "pentium3".into();
        }
        machine
    }

    fn ready_command(self) -> Vec<&'static str> {
        match self {
            Self::Temple => vec!["\"pc-agent-ready\\n\";"],
            Self::Dos => vec!["echo", "pc-agent-ready"],
            Self::Windows98 => vec!["COMMAND.COM", "/C", "echo pc-agent-ready"],
            _ => vec!["cmd.exe", "/C", "echo pc-agent-ready"],
        }
    }
}

struct Running {
    process: ManagedChild,
    control: Client,
    directory: tempfile::TempDir,
    guest: Guest,
    resumed: Instant,
}

impl Running {
    fn boot(guest: Guest) -> Option<Self> {
        let (arch, name, version, variable) = guest.template();
        if !Path::new("/dev/kvm").exists() {
            eprintln!("skipping pc {name}: /dev/kvm is unavailable");
            return None;
        }
        let template = if let Some(path) = std::env::var_os(variable) {
            let path = PathBuf::from(path);
            assert!(
                path.is_file(),
                "{variable} points at missing image {}",
                path.display()
            );
            path
        } else {
            let Some(home) = std::env::var_os("HOME") else {
                eprintln!("skipping pc {name}: HOME is unset; set {variable}");
                return None;
            };
            let path = PathBuf::from(home)
                .join(".local/share/vmlab/templates")
                .join(arch)
                .join(name)
                .join(version)
                .join("disk.qcow2");
            if !path.is_file() {
                eprintln!("skipping pc {name}: template {version} is missing; set {variable}");
                return None;
            }
            path
        };
        let bios = std::env::var_os("VMLAB_HV_SEABIOS")
            .map(PathBuf::from)
            .unwrap_or_else(|| "/usr/share/qemu/bios-256k.bin".into());
        let vga = std::env::var_os("VMLAB_HV_STDVGABIOS")
            .map(PathBuf::from)
            .unwrap_or_else(|| "/usr/share/qemu/vgabios-stdvga.bin".into());
        for path in [&bios, &vga] {
            if !path.is_file() {
                eprintln!("skipping pc {name}: firmware {} is missing", path.display());
                return None;
            }
        }
        let mut directory = tempfile::Builder::new()
            .prefix("hv-pc-")
            .tempdir_in("/var/tmp")
            .expect("short disk-backed pc acceptance directory");
        if std::env::var_os("VMLAB_HV_ACCEPTANCE_KEEP").is_some() {
            directory.disable_cleanup(true);
        }
        eprintln!("pc {name} artifacts: {}", directory.path().display());
        vmlab_disk::ops::create_linked_clone(&template, &directory.path().join("disk.qcow2"))
            .unwrap();
        let description = guest.description(directory.path(), &bios, &vga);
        let machine = directory.path().join("machine.json");
        std::fs::write(&machine, serde_json::to_vec_pretty(&description).unwrap()).unwrap();
        let binary = std::env::var_os("VMLAB_HV_BINARY")
            .map(PathBuf::from)
            .unwrap_or_else(vmlab_hv_tests::hv_binary);
        assert!(
            binary.is_file(),
            "build vmlab-hv first or set VMLAB_HV_BINARY: {}",
            binary.display()
        );
        let control_path = directory.path().join("control.sock");
        let error_log = std::fs::File::create(directory.path().join("stderr.log")).unwrap();
        let mut process = ManagedChild::spawn(
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
                .arg(binary)
                .arg("--machine")
                .arg(machine)
                .arg("--control")
                .arg(&control_path)
                .arg("--paused")
                .env("TMPDIR", directory.path())
                .env("VMLAB_HV_DIAGNOSTICS", "1")
                .stdout(Stdio::null())
                .stderr(error_log),
        )
        .expect("launch pc acceptance VM under its memory scope");
        let deadline = Instant::now() + Duration::from_secs(20);
        let control = loop {
            if let Ok(control) = Client::connect(&control_path) {
                break control;
            }
            assert!(
                process.try_wait().unwrap().is_none(),
                "pc {name} exited before control ready: {}",
                logs(directory.path())
            );
            assert!(
                Instant::now() < deadline,
                "pc {name} control timeout: {}",
                logs(directory.path())
            );
            std::thread::sleep(Duration::from_millis(20));
        };
        let mut running = Self {
            process,
            control,
            directory,
            guest,
            resumed: Instant::now(),
        };
        assert!(
            running.directory.path().join("agent.sock").exists(),
            "agent socket must bind before ready"
        );
        running.call(Control::Resume);
        Some(running)
    }

    fn call(&mut self, command: Control) {
        let outcome = self.control.call(command).expect("pc control reply");
        assert!(
            matches!(outcome, Outcome::Ok { .. }),
            "pc control operation: {outcome:?}"
        );
    }

    fn ready(&mut self) -> AgentClient {
        let mut agent = AgentClient::connect(
            &self.directory.path().join("agent.sock"),
            self.guest.timeout(),
        )
        .unwrap_or_else(|error| panic!("{:?} agent readiness: {error}", self.guest));
        assert!(
            agent
                .info()
                .features
                .iter()
                .any(|feature| feature == "exec"),
            "agent must advertise exec"
        );
        let expected = match self.guest {
            Guest::Temple => "templeos",
            Guest::Dos => "dos",
            _ => "windows",
        };
        assert!(
            agent.info().os.to_ascii_lowercase().contains(expected),
            "wrong guest agent: {:?}",
            agent.info()
        );
        let output = exec(&mut agent, &self.guest.ready_command());
        assert_eq!(clean(&output.stdout), "pc-agent-ready");
        eprintln!(
            "{:?} resume-to-agent-exec: {:.3} s",
            self.guest,
            self.resumed.elapsed().as_secs_f64()
        );
        agent
    }

    fn refresh(&mut self, agent: &mut AgentClient) {
        let deadline = Instant::now() + self.guest.timeout();
        loop {
            let last = match agent.resync(Duration::from_secs(3)) {
                Ok(()) => break,
                Err(error) => error,
            };
            assert!(
                self.process.try_wait().unwrap().is_none(),
                "VM exited while re-handshaking: {last}"
            );
            assert!(
                Instant::now() < deadline,
                "same agent socket did not re-handshake: {last}"
            );
        }
        let output = exec(agent, &self.guest.ready_command());
        assert_eq!(clean(&output.stdout), "pc-agent-ready");
    }

    fn reset(&mut self, agent: &mut AgentClient) {
        self.call(Control::Status);
        self.control.take_events();
        let pid = self.process.id();
        agent
            .shutdown("reboot", Duration::from_secs(20))
            .expect("guest acknowledged reboot");
        let deadline = Instant::now() + self.guest.timeout();
        loop {
            self.call(Control::Status);
            let events = self.control.take_events();
            assert!(
                !events
                    .iter()
                    .any(|event| matches!(event, Event::Shutdown { .. })),
                "reset exited VM: {events:?}"
            );
            if events.contains(&Event::Reset) {
                break;
            }
            assert!(Instant::now() < deadline, "guest reboot did not emit reset");
            std::thread::sleep(Duration::from_millis(50));
        }
        assert_eq!(self.process.id(), pid);
        assert!(
            self.process.try_wait().unwrap().is_none(),
            "in-place reset must retain process"
        );
        self.refresh(agent);
        let probe = match self.guest {
            Guest::Temple => agent.exec(&["\"%d\\n\",hv_pc_live_cookie;"], Duration::from_secs(30)),
            Guest::Dos => agent.exec(&["type", "C:\\HVPC.TXT"], Duration::from_secs(30)),
            _ => agent.exec(
                &["reg.exe", "query", "HKLM\\HARDWARE\\HVPC", "/v", "Cookie"],
                Duration::from_secs(30),
            ),
        }
        .expect("probe volatile boot state after reset");
        assert_ne!(
            probe.exit_code, 0,
            "reset left pre-reset boot state intact: {probe:?}"
        );
        if self.guest == Guest::Temple {
            assert!(
                clean(&probe.stdout).contains("hv_pc_live_cookie"),
                "reset probe failed for an unrelated reason: {probe:?}"
            );
        }
        if self.guest == Guest::Dos {
            shell(agent, self.guest, DOS_RESTORE_STARTUP);
        }
        self.call(Control::Status);
        eprintln!(
            "{:?}: reset retained process {pid}, control, and agent socket",
            self.guest
        );
    }

    fn poweroff(&mut self, agent: &mut AgentClient) {
        self.call(Control::Status);
        self.control.take_events();
        agent
            .shutdown("powerdown", Duration::from_secs(20))
            .expect("guest acknowledged powerdown");
        let deadline = Instant::now() + Duration::from_secs(90);
        let mut observed = false;
        while Instant::now() < deadline {
            match self
                .control
                .wait_event_timeout(deadline.saturating_duration_since(Instant::now()))
                .unwrap()
            {
                Some(Event::Shutdown {
                    reason: ShutdownReason::GuestShutdown,
                }) => {
                    observed = true;
                    break;
                }
                Some(Event::Reset) => panic!("powerdown unexpectedly rebooted guest"),
                Some(_) => {}
                None => break,
            }
        }
        assert!(observed, "guest powerdown did not emit guest_shutdown");
        loop {
            if let Some(status) = self.process.try_wait().unwrap() {
                assert!(status.success(), "guest powerdown exited {status}");
                break;
            }
            assert!(
                Instant::now() < deadline,
                "VM did not exit after guest shutdown"
            );
            std::thread::sleep(Duration::from_millis(20));
        }
    }
}

impl Drop for Running {
    fn drop(&mut self) {
        if std::thread::panicking() {
            let directory = self.directory.path();
            let _ = std::panic::catch_unwind(|| {
                crate::display_acceptance::diagnostic_screenshot(
                    &directory.join("display.sock"),
                    &directory.join("failure.ppm"),
                )
            });
            let _ = self.control.call(Control::Quit);
            let _ = self.process.wait();
            eprintln!(
                "{:?} acceptance failure: {}",
                self.guest,
                logs(self.directory.path())
            );
        }
    }
}

fn logs(directory: &Path) -> String {
    let read = |name| {
        let bytes = std::fs::read(directory.join(name)).unwrap_or_default();
        String::from_utf8_lossy(&bytes[bytes.len().saturating_sub(65_536)..]).into_owned()
    };
    format!(
        "stderr:\n{}\nserial:\n{}",
        read("stderr.log"),
        read("serial.log")
    )
}

fn clean(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes)
        .replace('\0', "")
        .trim()
        .to_string()
}

fn exec(agent: &mut AgentClient, arguments: &[&str]) -> ExecOutput {
    let output = agent
        .exec(arguments, Duration::from_secs(60))
        .unwrap_or_else(|error| panic!("guest exec {arguments:?}: {error}"));
    assert_eq!(output.exit_code, 0, "guest exec {arguments:?}: {output:?}");
    output
}

fn shell(agent: &mut AgentClient, guest: Guest, command: &str) -> ExecOutput {
    match guest {
        Guest::Temple => exec(agent, &[command]),
        Guest::Dos => {
            let arguments: Vec<_> = command.split_ascii_whitespace().collect();
            exec(agent, &arguments)
        }
        _ => exec(agent, &["cmd.exe", "/C", command]),
    }
}

fn snapshot(running: &mut Running, agent: &mut AgentClient) {
    let guest = running.guest;
    if guest == Guest::Dos {
        for command in DOS_BOOT_GUARD_COMMANDS {
            shell(agent, guest, command);
        }
    }
    let write_before = match guest {
        Guest::Temple => {
            "I64 hv_pc_live_cookie=0x12345678; FileWrite(\"~/HVPC.TXT\",\"before\",6);"
        }
        Guest::Dos => "echo before>C:\\HVPC.TXT",
        _ => {
            "reg add HKLM\\HARDWARE\\HVPC /v Cookie /t REG_SZ /d before /f && echo before>C:\\Windows\\Temp\\HVPC.TXT"
        }
    };
    let write_after = match guest {
        Guest::Temple => "hv_pc_live_cookie=0x87654321; FileWrite(\"~/HVPC.TXT\",\"after\",5);",
        Guest::Dos => "echo after>C:\\HVPC.TXT",
        _ => {
            "reg add HKLM\\HARDWARE\\HVPC /v Cookie /t REG_SZ /d after /f && echo after>C:\\Windows\\Temp\\HVPC.TXT"
        }
    };
    let read_marker = match guest {
        Guest::Temple => "{U8 *hv_p=FileRead(\"~/HVPC.TXT\"); \"%s\\n\",hv_p; Free(hv_p);}",
        Guest::Dos => "type C:\\HVPC.TXT",
        _ => "type C:\\Windows\\Temp\\HVPC.TXT",
    };
    shell(agent, guest, write_before);
    assert_eq!(clean(&shell(agent, guest, read_marker).stdout), "before");
    running.call(Control::Status);
    running.control.take_events();
    running.call(Control::SnapshotSave {
        name: "pc-p1".into(),
    });
    shell(agent, guest, write_after);
    assert_eq!(clean(&shell(agent, guest, read_marker).stdout), "after");
    let started = Instant::now();
    running.call(Control::SnapshotLoad {
        name: "pc-p1".into(),
    });
    running.refresh(agent);
    assert_eq!(
        clean(&shell(agent, guest, read_marker).stdout),
        "before",
        "disk state did not roll back"
    );
    match guest {
        Guest::Temple => {
            let cookie = shell(agent, guest, "\"%d\\n\",hv_pc_live_cookie;");
            assert_eq!(
                clean(&cookie.stdout),
                "305419896",
                "guest rebooted or RAM did not roll back"
            );
        }
        Guest::Windows7 | Guest::Windows98 | Guest::Vista => {
            let cookie = shell(agent, guest, "reg query HKLM\\HARDWARE\\HVPC /v Cookie");
            assert!(
                clean(&cookie.stdout).contains("before"),
                "volatile boot cookie was not restored"
            );
        }
        Guest::Dos => {
            // FDAUTO deletes the marker on every boot. Its presence above
            // proves the restored DOS guest did not take a cold-boot path.
        }
    }
    running.call(Control::Status);
    let events = running.control.take_events();
    assert!(
        !events
            .iter()
            .any(|event| matches!(event, Event::Reset | Event::Shutdown { .. })),
        "snapshot load rebooted or stopped guest: {events:?}"
    );
    running.call(Control::SnapshotDelete {
        name: "pc-p1".into(),
    });
    eprintln!(
        "{guest:?}: snapshot rolled back disk and live boot state in {:?}",
        started.elapsed()
    );
}

fn exercise(guest: Guest) {
    let Some(mut running) = Running::boot(guest) else {
        return;
    };
    let mut agent = running.ready();
    if guest == Guest::Windows7 {
        let report = shell(
            &mut agent,
            guest,
            "wmic path Win32_PnPEntity get Name,PNPDeviceID /format:csv",
        );
        let report = clean(&report.stdout);
        assert!(
            report.contains("ACPI"),
            "Windows HAL did not enumerate ACPI devices: {report}"
        );
        assert!(
            !report.to_ascii_uppercase().contains("VEN_15AD"),
            "pc unexpectedly exposes VMware PCI devices: {report}"
        );
        eprintln!("Windows 7 HAL/ACPI enumeration:\n{report}");
    }
    if !matches!(guest, Guest::Vista | Guest::Windows98) {
        snapshot(&mut running, &mut agent);
        running.reset(&mut agent);
    }
    running.poweroff(&mut agent);
}

#[test]
fn templeos_agent_snapshot_reset() {
    exercise(Guest::Temple);
}

#[test]
fn freedos_agent_snapshot_reset() {
    exercise(Guest::Dos);
}

#[test]
fn windows_7_agent_snapshot_reset() {
    exercise(Guest::Windows7);
}

#[test]
fn windows_vista_agent_smoke() {
    exercise(Guest::Vista);
}

#[test]
fn windows_98_a20_boot_cost_with_standard_vga() {
    let Some(mut running) = Running::boot(Guest::Windows98) else {
        return;
    };
    let _agent = running.ready();
    running.call(Control::Quit);
    assert!(running.process.wait().unwrap().success());
}

#[test]
fn pc_descriptions_use_explicit_ide_slots_and_the_gate_agent_transport() {
    for guest in [
        Guest::Temple,
        Guest::Dos,
        Guest::Windows7,
        Guest::Windows98,
        Guest::Vista,
    ] {
        let description: vmlab_hv_proto::machine::MachineDescription =
            serde_json::from_value(guest.description(
                Path::new("/var/tmp/pc"),
                Path::new("/bios"),
                Path::new("/vga"),
            ))
            .unwrap();
        assert_eq!(description.machine, vmlab_hv_proto::machine::Machine::Pc);
        assert!(description.nics.is_empty());
        assert_eq!(
            description.disks[0].bus,
            vmlab_hv_proto::machine::DiskBus::Ide
        );
        assert_eq!(description.disks[0].channel, Some(0));
        assert_eq!(description.disks[0].unit, Some(0));
        assert_eq!(description.ports[0].transport.is_virtio(), guest.windows());
        assert_eq!(description.serial_log_port.is_com1(), guest.windows());
    }
}

#[test]
fn freedos_reboot_guard_patches_the_startup_file_that_installs_the_agent() {
    assert_eq!(
        DOS_BOOT_GUARD_COMMANDS[0],
        "copy /Y C:\\FDAUTO.BAT C:\\HVAUTO.BAK"
    );
    assert_eq!(
        DOS_BOOT_GUARD_COMMANDS[1],
        "echo @del C:\\HVPC.TXT>C:\\HVAUTO.TMP"
    );
    assert_eq!(
        DOS_BOOT_GUARD_COMMANDS[2],
        "type C:\\FDAUTO.BAT>>C:\\HVAUTO.TMP"
    );
    assert_eq!(
        DOS_BOOT_GUARD_COMMANDS[3],
        "copy /Y C:\\HVAUTO.TMP C:\\FDAUTO.BAT"
    );
    assert_eq!(DOS_RESTORE_STARTUP, "copy /Y C:\\HVAUTO.BAK C:\\FDAUTO.BAT");
    assert!(
        DOS_BOOT_GUARD_COMMANDS
            .iter()
            .all(|command| !command.contains("AUTOEXEC"))
    );
    assert_eq!(Guest::Dos.ready_command(), ["echo", "pc-agent-ready"]);
}
