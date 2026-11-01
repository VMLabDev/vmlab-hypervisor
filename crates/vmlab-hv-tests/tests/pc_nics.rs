//! Phase 4b P1/S1: unchanged gate templates, networking, restore and reboot.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::json;
use vmlab_hv_proto::client::Client;
use vmlab_hv_proto::control::{Command as Ctrl, Event, Outcome};
use vmlab_hv_proto::machine::MachineDescription;
use vmlab_hv_tests::agent::AgentClient;
use vmlab_hv_tests::process::ManagedChild;
use vmlab_hv_tests::switch::MiniSwitch;

struct Vm {
    child: ManagedChild,
    dir: tempfile::TempDir,
    control: Client,
    network: MiniSwitch,
    dos: bool,
}

impl Drop for Vm {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        if std::thread::panicking() || std::env::var_os("VMLAB_HV_TEST_KEEP_PC").is_some() {
            self.dir.disable_cleanup(true);
            eprintln!(
                "retained pc NIC gate artifacts: {}",
                self.dir.path().display()
            );
        }
    }
}

impl Vm {
    fn diagnostics(&self) -> String {
        format!(
            "network={:?}\nhv.log={}\nserial.log={}",
            self.network.stats(),
            std::fs::read_to_string(self.dir.path().join("hv.log")).unwrap_or_default(),
            std::fs::read_to_string(self.dir.path().join("serial.log")).unwrap_or_default()
        )
    }
    fn call(&mut self, command: Ctrl) {
        let result = self.control.call(command).unwrap();
        assert!(
            matches!(result, Outcome::Ok { .. }),
            "{result:?}\n{}",
            self.diagnostics()
        );
    }
    fn agent(&self) -> AgentClient {
        AgentClient::connect(
            &self.dir.path().join("agent.sock"),
            Duration::from_secs(240),
        )
        .unwrap_or_else(|e| panic!("{e}\n{}", self.diagnostics()))
    }
    fn exec(&self, agent: &mut AgentClient, argv: &[&str]) -> String {
        let output = agent
            .exec(argv, Duration::from_secs(120))
            .unwrap_or_else(|e| panic!("{e}\n{}", self.diagnostics()));
        let text = format!(
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        eprintln!("exec {argv:?} exit={}\n{text}", output.exit_code);
        assert_eq!(
            output.exit_code,
            0,
            "{argv:?}: {text}\n{}",
            self.diagnostics()
        );
        text
    }
    fn ping(&self, agent: &mut AgentClient) {
        let before = self.network.stats().icmp_echo_replies;
        let output = if self.dos {
            self.exec(agent, &["C:\\VMLAB\\P4PING.BAT"])
        } else {
            self.exec(agent, &["ping.exe", "-n", "2", "-w", "5000", "10.42.0.1"])
        };
        assert!(
            self.network.stats().icmp_echo_replies >= before + 2,
            "no ICMP exchange: {output}\n{}",
            self.diagnostics()
        );
        assert!(
            output.contains("Lost = 0") || output.contains("Replies lost: 0"),
            "guest did not confirm replies: {output}"
        );
    }

    fn load_packet_driver(&self, agent: &mut AgentClient) {
        let driver = agent
            .exec(
                &["C:\\NET\\FDNET\\PCNTPK.COM", "INT=0x60"],
                Duration::from_secs(30),
            )
            .unwrap();
        eprintln!(
            "packet driver termination={} output={}",
            driver.exit_code,
            String::from_utf8_lossy(&driver.stdout)
        );
        // DOS reports INT 21h/AH=31h as termination type 3 in the high
        // byte. A TSR with low-byte status zero is installed successfully.
        assert!(
            matches!(driver.exit_code, 0 | 0x300),
            "packet driver failed: {driver:?}"
        );
        assert!(
            String::from_utf8_lossy(&driver.stdout)
                .contains("My Ethernet address is 52:54:00:12:34:56")
        );
    }

    fn marker(&self, agent: &mut AgentClient, value: &str) {
        if self.dos {
            self.exec(agent, &["echo", value, ">C:\\P4NIC.TXT"]);
        } else {
            self.exec(agent, &["cmd.exe","/C",&format!("reg add HKLM\\HARDWARE\\P4NIC /v Cookie /t REG_SZ /d {value} /f && echo {value}>C:\\Windows\\Temp\\P4NIC.TXT")]);
        }
    }

    fn reboot(&mut self, agent: &mut AgentClient) {
        self.call(Ctrl::Status);
        self.control.take_events();
        let pid = self.child.id();
        agent.shutdown("reboot", Duration::from_secs(30)).unwrap();
        let deadline = Instant::now() + Duration::from_secs(240);
        loop {
            let event = self
                .control
                .wait_event_timeout(deadline.saturating_duration_since(Instant::now()))
                .unwrap();
            if event == Some(Event::Reset) {
                break;
            }
            assert!(
                event.is_some() && Instant::now() < deadline,
                "guest did not reset\n{}",
                self.diagnostics()
            );
            assert!(
                !matches!(event, Some(Event::Shutdown { .. })),
                "guest reset exited the VM"
            );
        }
        assert_eq!(self.child.id(), pid);
        assert!(self.child.try_wait().unwrap().is_none());
        loop {
            if agent.resync(Duration::from_secs(3)).is_ok() {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "agent did not return after reboot\n{}",
                self.diagnostics()
            );
        }
    }

    fn settle_windows_setup(&mut self, agent: &mut AgentClient) {
        let deadline = Instant::now() + Duration::from_secs(600);
        loop {
            let output = agent.exec(&["cmd.exe","/C","reg query HKLM\\SYSTEM\\Setup /v SystemSetupInProgress & reg query HKLM\\SYSTEM\\Setup /v OOBEInProgress"], Duration::from_secs(20));
            if let Ok(output) = &output {
                let text = String::from_utf8_lossy(&output.stdout);
                eprintln!("Windows setup: {text}");
                if ["SystemSetupInProgress", "OOBEInProgress"]
                    .iter()
                    .all(|name| {
                        text.lines().any(|line| {
                            let fields: Vec<_> = line.split_ascii_whitespace().collect();
                            fields == [*name, "REG_DWORD", "0x0"]
                        })
                    })
                {
                    break;
                }
            }
            assert!(
                Instant::now() < deadline,
                "Windows setup did not finish: {output:?}"
            );
            if output.is_err() {
                let _ = agent.resync(Duration::from_secs(3));
            }
            std::thread::sleep(Duration::from_secs(1));
        }
        self.reboot(agent);
    }
}

fn template(dos: bool) -> PathBuf {
    let (variable, suffix) = if dos {
        ("VMLAB_HV_TEST_FREEDOS", "x86/freedos-1.3/1.3.0/disk.qcow2")
    } else {
        (
            "VMLAB_HV_TEST_WINDOWS7",
            "x86_64/windows-7/7600.0.1/disk.qcow2",
        )
    };
    if let Some(path) = std::env::var_os(variable) {
        return PathBuf::from(path);
    }
    PathBuf::from(std::env::var_os("HOME").unwrap())
        .join(".local/share/vmlab/templates")
        .join(suffix)
}

fn launch(dos: bool) -> Option<Vm> {
    let firmware = Path::new("/usr/share/qemu/bios-256k.bin");
    for file in [
        Path::new("/dev/kvm"),
        firmware,
        Path::new("/usr/share/qemu/vgabios-stdvga.bin"),
        &template(dos),
    ] {
        if !file.exists() {
            eprintln!("SKIP pc NIC gate: missing {}", file.display());
            return None;
        }
    }
    let dir = tempfile::Builder::new()
        .prefix("hv-pc-nics-")
        .tempdir_in("/var/tmp")
        .unwrap();
    let path = dir.path();
    let overlay = path.join("disk.qcow2");
    vmlab_disk::ops::create_linked_clone(&template(dos), &overlay).unwrap();
    let network = MiniSwitch::bind(&path.join("switch.sock")).unwrap();
    let description: MachineDescription = serde_json::from_value(json!({
        "name":"pc-nics", "machine":"pc", "arch":"x86_64", "accel":"kvm",
        "vcpus":if dos { 1 } else { 2 }, "memory_mib":if dos { 64 } else { 4096 },
        "hyperv":if dos { vec![] } else { vec!["relaxed","vapic","spinlocks","vpindex","runtime","time","synic","stimer","frequencies","reset","tlbflush","ipi","avic"] },
        "boot":{"kind":"firmware","firmware":"seabios","code":firmware},
        "console_pci":if dos { serde_json::Value::Null } else { json!({"bus":0,"device":2,"function":0}) },
        "display":{"device":"std-vga","pci":{"bus":0,"device":if dos { 3 } else { 4 },"function":0},"rom":null},
        "rtc_base":if dos { json!("localtime") } else { serde_json::Value::Null },
        "disks":[{"id":"disk0","bus":"ide","channel":0,"unit":0,"path":overlay,"format":"qcow2","read_only":false,"vmstate":true}],
        "nics":[{"model":if dos { "pcnet" } else { "e1000" },"pci":{"bus":0,"device":if dos { 2 } else { 3 },"function":0},
            "mac":vmlab_hv_tests::mac_string(vmlab_hv_tests::TEST_MAC),"attachment":{"kind":"stream","socket":path.join("switch.sock")}}],
        "serial_log":path.join("serial.log"), "serial_log_port":if dos { "com2" } else { "com1" },
        "ports":[{"transport":if dos { "com1" } else { "virtio" },"name":"vmlab.agent.0","socket":path.join("agent.sock")}]
    })).unwrap();
    std::fs::write(
        path.join("machine.json"),
        serde_json::to_vec_pretty(&description).unwrap(),
    )
    .unwrap();
    let log = std::fs::File::create(path.join("hv.log")).unwrap();
    let mut child = ManagedChild::spawn(
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
            .arg(path.join("machine.json"))
            .arg("--control")
            .arg(path.join("control.sock"))
            .env("TMPDIR", path)
            .stdout(Stdio::from(log.try_clone().unwrap()))
            .stderr(Stdio::from(log)),
    )
    .unwrap();
    let deadline = Instant::now() + Duration::from_secs(30);
    let control = loop {
        if let Ok(control) = Client::connect(path.join("control.sock")) {
            break control;
        }
        assert!(
            Instant::now() < deadline && child.try_wait().unwrap().is_none(),
            "pc build failed: {}",
            std::fs::read_to_string(path.join("hv.log")).unwrap()
        );
        std::thread::sleep(Duration::from_millis(25));
    };
    Some(Vm {
        child,
        dir,
        control,
        network,
        dos,
    })
}

fn network_gate(dos: bool) {
    let Some(mut vm) = launch(dos) else {
        return;
    };
    let mut agent = vm.agent();
    eprintln!("guest ready: {:?}", agent.info());
    if !dos {
        vm.settle_windows_setup(&mut agent);
    }
    if dos {
        vm.load_packet_driver(&mut agent);
        // A batch file keeps MTCPCFG in scope for both mTCP programs. Echo
        // tokens are separate argv entries because DOS uses COMMAND.COM.
        vm.exec(
            &mut agent,
            &[
                "echo",
                "SET",
                "MTCPCFG=C:\\NET\\FDNET\\MTCP.CFG",
                ">C:\\VMLAB\\P4DHCP.BAT",
            ],
        );
        vm.exec(
            &mut agent,
            &[
                "echo",
                "C:\\NET\\FDNET\\DHCP.EXE",
                ">>C:\\VMLAB\\P4DHCP.BAT",
            ],
        );
        vm.exec(&mut agent, &["C:\\VMLAB\\P4DHCP.BAT"]);
        vm.exec(
            &mut agent,
            &[
                "echo",
                "SET",
                "MTCPCFG=C:\\NET\\FDNET\\MTCP.CFG",
                ">C:\\VMLAB\\P4PING.BAT",
            ],
        );
        vm.exec(
            &mut agent,
            &[
                "echo",
                "C:\\NET\\MTCP\\PING.EXE",
                "-count",
                "2",
                "10.42.0.1",
                ">>C:\\VMLAB\\P4PING.BAT",
            ],
        );
    } else {
        vm.exec(&mut agent, &["ipconfig.exe", "/release"]);
        let config = vm.exec(&mut agent, &["ipconfig.exe", "/renew"]);
        assert!(config.contains("10.42.0.50"), "no DHCP lease: {config}");
    }
    let stats = vm.network.stats();
    assert!(
        stats.dhcp_acked && stats.dhcp_discover && stats.dhcp_request,
        "DHCP exchange missing: {stats:?}"
    );
    vm.ping(&mut agent);
    if dos {
        for argv in [
            vec!["copy", "/Y", "C:\\FDAUTO.BAT", "C:\\P4AUTO.BAK"],
            vec!["echo", "@del", "C:\\P4NIC.TXT", ">C:\\P4AUTO.TMP"],
            vec!["type", "C:\\FDAUTO.BAT", ">>C:\\P4AUTO.TMP"],
            vec!["copy", "/Y", "C:\\P4AUTO.TMP", "C:\\FDAUTO.BAT"],
        ] {
            vm.exec(&mut agent, &argv);
        }
    }
    vm.marker(&mut agent, "before");
    vm.call(Ctrl::Status);
    vm.control.take_events();
    vm.call(Ctrl::SnapshotSave {
        name: "network".into(),
    });
    vm.marker(&mut agent, "after");
    vm.ping(&mut agent);
    vm.call(Ctrl::Pause);
    vm.call(Ctrl::SnapshotLoad {
        name: "network".into(),
    });
    vm.call(Ctrl::Resume);
    agent.resync(Duration::from_secs(30)).unwrap();
    let marker = if dos {
        vm.exec(&mut agent, &["type", "C:\\P4NIC.TXT"])
    } else {
        vm.exec(
            &mut agent,
            &[
                "cmd.exe",
                "/C",
                "type C:\\Windows\\Temp\\P4NIC.TXT && reg query HKLM\\HARDWARE\\P4NIC /v Cookie",
            ],
        )
    };
    assert!(
        marker.contains("before") && !marker.contains("after"),
        "snapshot did not restore disk and boot state: {marker}"
    );
    vm.call(Ctrl::Status);
    assert!(
        !vm.control
            .take_events()
            .iter()
            .any(|event| matches!(event, Event::Reset | Event::Shutdown { .. })),
        "restore rebooted the guest"
    );
    vm.ping(&mut agent);
    vm.call(Ctrl::SnapshotDelete {
        name: "network".into(),
    });
    vm.reboot(&mut agent);
    if dos {
        let marker = agent
            .exec(&["type", "C:\\P4NIC.TXT"], Duration::from_secs(10))
            .unwrap();
        assert!(
            !String::from_utf8_lossy(&marker.stdout).contains("before"),
            "reboot guard did not execute"
        );
        vm.load_packet_driver(&mut agent);
    } else {
        let marker = agent
            .exec(
                &["reg.exe", "query", "HKLM\\HARDWARE\\P4NIC", "/v", "Cookie"],
                Duration::from_secs(10),
            )
            .unwrap();
        assert_eq!(
            marker.exit_code, 1,
            "volatile cookie survived reboot: {marker:?}"
        );
    }
    vm.ping(&mut agent);
    vm.call(Ctrl::Quit);
    assert!(vm.child.wait().unwrap().success());
}

#[test]
fn windows7_e1000_dhcp_ping_snapshot_reset() {
    network_gate(false);
}

#[test]
fn freedos_pcnet_packet_driver_dhcp_ping_snapshot_reset() {
    network_gate(true);
}
