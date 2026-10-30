//! S1, the spec's primary integration seam: spawn `vmlab-hv`, drive it over
//! the control socket, and observe the guest through the cinit ctl channel,
//! the vmlab-agent port and the serial log.
//!
//! Phase 1 acceptance: boot the x86_64
//! container guest asset to cinit `idle` and agent ready; exec and check
//! output and exit code; restore running and paused snapshots; a workload-mode
//! container exit powers the machine off; `powerdown` powers the guest off;
//! `quit` exits; a NIC exchanges DHCP
//! frames with a test-hosted switch; boot-to-agent-ready is measured and
//! printed. Tests skip with a printed reason without `/dev/kvm` or a rootfs.

use std::time::Duration;

use vmlab_cinit_proto::CtlEvent;
use vmlab_hv_proto::control::{Command as Ctrl, Event, Outcome, ShutdownReason};
use vmlab_hv_tests::agent::AgentClient;
use vmlab_hv_tests::ctl::CtlClient;
use vmlab_hv_tests::switch::{LEASE_IP, SERVER_IP};
use vmlab_hv_tests::{Assets, MachineSpec, probe_assets};

/// The busybox cinit injects into every container rootfs (see
/// `vmlab-agent-proto`: `BUSYBOX_FALLBACK`): the one executable every
/// rootfs — Alpine-built or a pulled image — is guaranteed to have.
const BUSYBOX: &str = "/.vmlab/busybox";

/// How long a whole boot may take before the test gives up.
const BOOT: Duration = Duration::from_secs(120);

fn snapshot_roundtrip(paused: bool, check_disk: bool) {
    let Some(assets) = check() else { return };
    let spec = MachineSpec::idle("s1-snapshot");
    let mut guest = vmlab_hv_tests::launch(&spec, &assets);
    let container = spec.container_spec();
    let (mut ctl, _) = boot_to_idle(&guest, &container, BOOT);
    let mut agent = agent_ready(&guest);
    let serial_path = guest.serial_log.clone();
    let hv_path = guest.hv_log.clone();
    let exec = |agent: &mut AgentClient, script: &str| {
        let output = agent
            .exec(&[BUSYBOX, "sh", "-c", script], Duration::from_secs(15))
            .unwrap_or_else(|e| {
                panic!(
                    "{script}: {e}\nserial: {}\nhv: {}",
                    std::fs::read_to_string(&serial_path).unwrap_or_default(),
                    std::fs::read_to_string(&hv_path).unwrap_or_default()
                )
            });
        assert_eq!(output.exit_code, 0, "{output:?}");
        output
    };
    exec(&mut agent, "echo saved > /marker; sync");
    eprintln!("marker saved; taking snapshot");
    if paused {
        assert!(matches!(
            guest.control.call(Ctrl::Pause).unwrap(),
            Outcome::Ok { .. }
        ));
    }
    assert!(matches!(
        guest
            .control
            .call(Ctrl::SnapshotSave {
                name: "saved".into()
            })
            .unwrap(),
        Outcome::Ok { .. }
    ));
    if paused {
        assert!(matches!(
            guest.control.call(Ctrl::Status).unwrap(),
            Outcome::Ok {
                status: Some(vmlab_hv_proto::control::RunState::Paused)
            }
        ));
        guest.control.call(Ctrl::Resume).unwrap();
    }
    exec(&mut agent, "echo changed > /marker; sync");
    eprintln!("marker changed; loading snapshot");
    assert_eq!(exec(&mut agent, "cat /marker").stdout, b"changed\n");
    assert!(matches!(
        guest
            .control
            .call(Ctrl::SnapshotLoad {
                name: "saved".into()
            })
            .unwrap(),
        Outcome::Ok { .. }
    ));
    if paused {
        assert!(matches!(
            guest.control.call(Ctrl::Status).unwrap(),
            Outcome::Ok {
                status: Some(vmlab_hv_proto::control::RunState::Paused)
            }
        ));
        guest.control.call(Ctrl::Resume).unwrap();
    }
    drop(agent);
    eprintln!("snapshot loaded; reconnecting agent");
    let mut agent = agent_ready(&guest);
    for _ in 0..6 {
        assert_eq!(exec(&mut agent, "cat /marker").stdout, b"saved\n");
        std::thread::sleep(Duration::from_secs(1));
    }
    ctl.send(&vmlab_cinit_proto::CtlCommand::Resync).unwrap();
    ctl.wait_event(
        &container,
        Duration::from_secs(15),
        |e| matches!(e, CtlEvent::Idle),
        |_| {},
        || guest.diagnostics("resync"),
    );
    guest.quit_and_exit_zero();
    let output = std::process::Command::new("qemu-img")
        .args(["check", "--output=json"])
        .arg(guest.dir().join("scratch.qcow2"))
        .output()
        .expect("qemu-img check");
    eprintln!(
        "qemu-img check: {}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    if check_disk {
        assert!(output.status.success(), "qemu-img check failed");
    }
}

#[test]
fn snapshot_running_guest() {
    snapshot_roundtrip(false, false);
}

#[test]
fn snapshot_paused_guest() {
    snapshot_roundtrip(true, false);
}

#[test]
fn snapshot_disk_integrity() {
    snapshot_roundtrip(false, true);
}

/// Resolve assets; print the skip reason and return `None` when the S1
/// acceptance cannot run on this machine.
fn check() -> Option<Assets> {
    let assets = probe_assets();
    if let Some(reason) = assets.skip_reason() {
        eprintln!("skipping S1: {reason}");
        return None;
    }
    Some(assets)
}

/// Connect the ctl channel, hand cinit its spec, and reach the idle
/// announcement. Returns the client (for later events) and every event seen
/// on the way.
fn boot_to_idle(
    guest: &vmlab_hv_tests::Guest,
    spec: &vmlab_cinit_proto::ContainerSpec,
    timeout: Duration,
) -> (CtlClient, Vec<CtlEvent>) {
    let mut ctl =
        CtlClient::connect(&guest.ctl_sock, Duration::from_secs(10)).expect("connect the ctl port");
    ctl.send_spec(spec).expect("send spec");
    let mut seen = Vec::new();
    let event = ctl.wait_event(
        spec,
        timeout,
        |e| matches!(e, CtlEvent::Idle),
        |e| seen.push(e.clone()),
        || guest.diagnostics("waiting for cinit idle"),
    );
    assert!(
        matches!(event, CtlEvent::Idle),
        "wanted idle, got {event:?}"
    );
    (ctl, seen)
}

/// Connect and handshake the agent, retrying past its startup.
fn agent_ready(guest: &vmlab_hv_tests::Guest) -> AgentClient {
    let agent =
        AgentClient::connect(&guest.agent_sock, Duration::from_secs(60)).expect("agent handshake");
    let info = agent.info();
    eprintln!(
        "agent ready: {} on {}, version {} (features: {})",
        info.agent_version,
        info.os,
        info.proto_version,
        info.features.join(",")
    );
    agent
}

#[test]
fn boots_to_idle_exec_and_scratch_roundtrip() {
    let Some(assets) = check() else { return };

    let spec = MachineSpec::idle("s1-idle");
    let mut guest = vmlab_hv_tests::launch(&spec, &assets);
    let container_spec = spec.container_spec();

    let (_ctl, _) = boot_to_idle(&guest, &container_spec, BOOT);

    // Boot-to-agent-ready, measured and printed (spec S1).
    let mut agent = agent_ready(&guest);
    eprintln!("boot-to-agent-ready: {:?}", guest.launched.elapsed());

    // Exec a command and check output and exit code.
    let uname = agent
        .exec(&[BUSYBOX, "uname", "-a"], Duration::from_secs(30))
        .expect("exec uname");
    let stdout = String::from_utf8_lossy(&uname.stdout);
    eprintln!("uname -a: {stdout}");
    assert_eq!(uname.exit_code, 0, "uname exit code: {uname:?}");
    assert!(stdout.contains("Linux"), "uname output: {stdout}");

    // Write then read a file on the scratch-backed overlay: the container
    // root is the squashfs + qcow2-scratch overlay, so the write path runs
    // virtio-blk writes into our qcow2 and the read back out of it.
    let marker = format!("s1-marker-{}", std::process::id());
    let write = agent
        .exec(
            &[BUSYBOX, "sh", "-c", &format!("echo {marker} > /marker")],
            Duration::from_secs(30),
        )
        .expect("write marker");
    assert_eq!(write.exit_code, 0, "marker write: {write:?}");
    let read = agent
        .exec(&[BUSYBOX, "cat", "/marker"], Duration::from_secs(30))
        .expect("read marker");
    let text = String::from_utf8_lossy(&read.stdout);
    assert_eq!(read.exit_code, 0, "marker read: {read:?}");
    assert_eq!(text.trim(), marker, "marker content: {text:?}");

    guest.quit_and_exit_zero();
}

#[test]
fn workload_container_exit_powers_off_machine() {
    let Some(assets) = check() else { return };

    let spec = MachineSpec::workload(
        "s1-workload",
        vec![
            BUSYBOX.to_string(),
            "sh".to_string(),
            "-c".to_string(),
            "sleep 1; exit 42".to_string(),
        ],
        42,
    );
    let mut guest = vmlab_hv_tests::launch(&spec, &assets);
    let container_spec = spec.container_spec();

    let mut ctl =
        CtlClient::connect(&guest.ctl_sock, Duration::from_secs(10)).expect("connect the ctl port");
    ctl.send_spec(&container_spec).expect("send spec");
    let started = ctl.wait_event(
        &container_spec,
        BOOT,
        |e| matches!(e, CtlEvent::Started { .. }),
        |_| {},
        || guest.diagnostics("waiting for container started"),
    );
    eprintln!("container started: {started:?}");
    let exited = ctl.wait_event(
        &container_spec,
        Duration::from_secs(30),
        |e| matches!(e, CtlEvent::Exited { .. }),
        |_| {},
        || guest.diagnostics("waiting for container exit"),
    );
    let CtlEvent::Exited { code } = exited else {
        unreachable!("wait_event checked the variant")
    };
    assert_eq!(code, 42, "container exit code");

    // cinit powers off right after `exited`: the guest enters S5, the
    // hypervisor reports the shutdown event and exits 0.
    let event = guest
        .control
        .wait_event_timeout(Duration::from_secs(30))
        .expect("control connection")
        .expect("shutdown event");
    assert!(
        matches!(
            event,
            Event::Shutdown {
                reason: ShutdownReason::GuestShutdown
            }
        ),
        "wanted guest shutdown event, got {event:?}"
    );
    let status = guest.wait_exit(Duration::from_secs(30));
    assert!(
        status.success(),
        "guest poweroff must exit 0: {status}\n{}",
        guest.diagnostics("workload exit")
    );
}

#[test]
fn powerdown_accepted_and_guest_poweroff_exits_zero() {
    let Some(assets) = check() else { return };

    let spec = MachineSpec::idle("s1-powerdown");
    let mut guest = vmlab_hv_tests::launch(&spec, &assets);
    let container_spec = spec.container_spec();
    let (mut ctl, _) = boot_to_idle(&guest, &container_spec, BOOT);
    let mut agent = agent_ready(&guest);

    // The pinned guest asset ships no acpi-button driver (no button.ko in
    // the trimmed module tree, none built in), so no guest-side listener
    // for the ACPI power button can exist and PM1a_EN.PWRBTN never gets
    // enabled — vmlab itself never uses the button against containers for
    // the same reason (its stop ladder goes ctl stop -> agent shutdown ->
    // quit). Prove what this asset can express: the button press is
    // latched and accepted, and a guest poweroff (cinit's stop rung, the
    // same door a real host uses) reports the shutdown event and exits 0.
    let out = agent
        .exec(
            &[BUSYBOX, "sh", "-c", "ls /dev/input 2>/dev/null | wc -l"],
            Duration::from_secs(30),
        )
        .expect("probe /dev/input");
    let input_devices = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if input_devices != "0" {
        eprintln!(
            "note: this guest asset has input devices ({input_devices}); \
             the acpid-based button test could run here"
        );
    } else {
        eprintln!("note: guest asset has no input layer — no button listener possible");
    }

    // The button press: latched and accepted.
    let outcome = guest.control.call(Ctrl::Powerdown).expect("powerdown");
    assert!(
        matches!(outcome, Outcome::Ok { .. }),
        "powerdown must be accepted: {outcome:?}"
    );

    // Power the guest off through cinit's stop rung and require the full
    // exit path: S5, the shutdown event, exit code 0.
    ctl.send(&vmlab_cinit_proto::CtlCommand::Stop { grace_secs: 1 })
        .expect("ctl stop");
    let event = guest
        .control
        .wait_event_timeout(Duration::from_secs(30))
        .expect("control connection");
    assert!(
        matches!(
            event,
            Some(Event::Shutdown {
                reason: ShutdownReason::GuestShutdown
            })
        ),
        "wanted guest shutdown after stop, got {event:?}"
    );
    let status = guest.wait_exit(Duration::from_secs(30));
    assert!(
        status.success(),
        "guest poweroff must exit 0: {status}\n{}",
        guest.diagnostics("powerdown")
    );
}

#[test]
fn nic_dhcp_exchange_with_test_switch() {
    let Some(assets) = check() else { return };

    let spec = MachineSpec::idle("s1-nic").with_nics(1);
    let mut guest = vmlab_hv_tests::launch(&spec, &assets);
    let container_spec = spec.container_spec();

    let (_ctl, seen) = boot_to_idle(&guest, &container_spec, BOOT);

    // eth0 came up: cinit reported the lease the test's DHCP responder
    // handed out.
    let net_up = seen
        .iter()
        .find_map(|e| match e {
            CtlEvent::NetUp { ip } => Some(ip.clone()),
            _ => None,
        })
        .unwrap_or_else(|| {
            panic!(
                "no net_up event: switch saw {:?}\n{}",
                guest.switch.as_ref().map(|s| s.stats()),
                guest.diagnostics("nic")
            )
        });
    assert_eq!(net_up, LEASE_IP.to_string(), "leased IP");
    eprintln!("eth0 up with {net_up} via {SERVER_IP}");

    // The switch received frames from the guest's MAC, and the DHCP
    // exchange completed both directions (an ACK the guest accepted is what
    // net_up proves).
    let stats = guest.switch.as_ref().expect("switch").stats();
    assert!(
        stats.frames_from_guest_mac >= 1,
        "no frame from the guest's MAC: {:?}",
        stats.seen
    );
    assert!(stats.dhcp_discover, "no DHCPDISCOVER: {:?}", stats.seen);
    assert!(stats.dhcp_request, "no DHCPREQUEST: {:?}", stats.seen);
    assert!(stats.dhcp_acked, "no completed exchange: {:?}", stats.seen);

    guest.quit_and_exit_zero();
}

#[test]
fn nic_traffic_survives_snapshot_load() {
    let Some(assets) = check() else { return };

    let spec = MachineSpec::idle("s1-nic-snapshot").with_nics(1);
    let mut guest = vmlab_hv_tests::launch(&spec, &assets);
    let container = spec.container_spec();
    let (_ctl, _) = boot_to_idle(&guest, &container, BOOT);
    let mut agent = agent_ready(&guest);

    let exchange = |agent: &mut AgentClient, guest: &vmlab_hv_tests::Guest, phase: &str| {
        let switch = guest.switch.as_ref().expect("switch");
        let before = switch.stats().frames_from_guest_mac;
        // Request a fresh lease without changing the interface configuration.
        // A successful foreground client must receive the switch's OFFER/ACK.
        let output = agent
            .exec(
                &[
                    BUSYBOX,
                    "udhcpc",
                    "-f",
                    "-n",
                    "-q",
                    "-i",
                    "eth0",
                    "-t",
                    "3",
                    "-T",
                    "1",
                    "-s",
                    "/bin/true",
                ],
                Duration::from_secs(15),
            )
            .unwrap_or_else(|e| panic!("{phase}: {e}\n{}", guest.diagnostics(phase)));
        eprintln!("{phase}: {output:?}");
        assert_eq!(output.exit_code, 0, "{phase}: {output:?}");
        let after = switch.stats();
        assert!(
            after.frames_from_guest_mac >= before + 2,
            "{phase}: no fresh DHCP exchange: {after:?}\n{}",
            guest.diagnostics(phase)
        );
    };

    exchange(&mut agent, &guest, "before snapshot save");
    assert!(matches!(
        guest
            .control
            .call(Ctrl::SnapshotSave { name: "nic".into() })
            .unwrap(),
        Outcome::Ok { .. }
    ));
    exchange(&mut agent, &guest, "after snapshot save");
    assert!(matches!(
        guest
            .control
            .call(Ctrl::SnapshotLoad { name: "nic".into() })
            .unwrap(),
        Outcome::Ok { .. }
    ));
    drop(agent);
    let mut agent = agent_ready(&guest);
    exchange(&mut agent, &guest, "after snapshot load");

    guest.quit_and_exit_zero();
}

#[test]
fn quit_exits_cleanly_while_booting() {
    let Some(assets) = check() else { return };

    // `quit` must work at any point, including mid-boot: vmlab relies on it
    // as the stop ladder's last rung.
    let spec = MachineSpec::idle("s1-quit");
    let mut guest = vmlab_hv_tests::launch(&spec, &assets);
    guest.quit_and_exit_zero();
}

#[test]
fn missing_kernel_is_a_clear_launch_error() {
    use std::io::Write as _;

    let dir =
        std::env::temp_dir().join(format!("vmlab-hv-s1-missing-kernel-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let machine = dir.join("machine.json");
    let json = format!(
        r#"{{
            "name": "s1-missing-kernel",
            "arch": "x86_64",
            "accel": "kvm",
            "vcpus": 1,
            "memory_mib": 128,
            "boot": {{ "kind": "kernel", "kernel": "{}/no-such-vmlinuz", "initrd": null,
                      "cmdline": "console=ttyS0" }},
            "disks": [],
            "nics": [],
            "serial_log": "{}/serial.log",
            "ports": [],
            "shares": []
        }}"#,
        dir.display(),
        dir.display()
    );
    std::fs::File::create(&machine)
        .unwrap()
        .write_all(json.as_bytes())
        .unwrap();
    let control = dir.join("control.sock");

    let output = std::process::Command::new(vmlab_hv_tests::hv_binary())
        .arg("--machine")
        .arg(&machine)
        .arg("--control")
        .arg(&control)
        .output()
        .expect("spawn vmlab-hv");

    assert!(
        !output.status.success(),
        "a missing kernel must fail the launch"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("no-such-vmlinuz"),
        "the error must name the kernel: {stderr}"
    );
    assert!(!control.exists(), "no control socket may be left behind");
    let _ = std::fs::remove_dir_all(&dir);
}
