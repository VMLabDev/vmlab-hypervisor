//! S1, the spec's primary integration seam: spawn `vmlab-hv`, drive it over
//! the control socket, and observe the guest through the cinit ctl channel,
//! the vmlab-agent port and the serial log.
//!
//! Phase 1 acceptance (minus snapshots, owned elsewhere): boot the x86_64
//! container guest asset to cinit `idle` and agent ready; exec and check
//! output and exit code; a workload-mode container exit powers the machine
//! off; `powerdown` powers the guest off; `quit` exits; a NIC exchanges DHCP
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
    let mut ctl = CtlClient::connect(&guest.ctl_sock, Duration::from_secs(10))
        .expect("connect the ctl port");
    ctl.send_spec(spec).expect("send spec");
    let mut seen = Vec::new();
    let event = ctl.wait_event(
        spec,
        timeout,
        |e| matches!(e, CtlEvent::Idle),
        |e| seen.push(e.clone()),
        || guest.diagnostics("waiting for cinit idle"),
    );
    assert!(matches!(event, CtlEvent::Idle), "wanted idle, got {event:?}");
    (ctl, seen)
}

/// Connect and handshake the agent, retrying past its startup.
fn agent_ready(guest: &vmlab_hv_tests::Guest) -> AgentClient {
    let agent = AgentClient::connect(&guest.agent_sock, Duration::from_secs(60))
        .expect("agent handshake");
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
    eprintln!(
        "boot-to-agent-ready: {:?}",
        guest.launched.elapsed()
    );

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

    let mut ctl = CtlClient::connect(&guest.ctl_sock, Duration::from_secs(10))
        .expect("connect the ctl port");
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
        .wait_event()
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
fn powerdown_over_control_socket_powers_off_guest() {
    let Some(assets) = check() else { return };

    let spec = MachineSpec::idle("s1-powerdown");
    let mut guest = vmlab_hv_tests::launch(&spec, &assets);
    let container_spec = spec.container_spec();
    let (_ctl, _) = boot_to_idle(&guest, &container_spec, BOOT);
    let mut agent = agent_ready(&guest);

    // The guest asset ships no acpid (nothing in a container micro-VM
    // listens for the ACPI power button), so the harness installs the
    // headless-server listener: busybox acpid with a one-line action file
    // that forces a poweroff. This is exactly how a real host without
    // systemd makes the button work.
    let install = [
        format!("{BUSYBOX} mkdir -p /etc/acpi"),
        format!("echo '{BUSYBOX} poweroff -f' > /etc/acpi/powerbtn.sh"),
        format!("{BUSYBOX} chmod +x /etc/acpi/powerbtn.sh"),
    ]
    .join(" && ");
    let out = agent
        .exec(&[BUSYBOX, "sh", "-c", &install], Duration::from_secs(30))
        .expect("install acpid hook");
    assert_eq!(out.exit_code, 0, "acpid hook install: {out:?}");

    // acpid in its own session so closing the exec channel does not kill
    // it; the exec returns once the daemon is running detached.
    let start = format!("{BUSYBOX} setsid {BUSYBOX} acpid 2>/dev/null");
    let out = agent
        .exec(&[BUSYBOX, "sh", "-c", &start], Duration::from_secs(30))
        .expect("start acpid");
    assert_eq!(out.exit_code, 0, "acpid start: {out:?}");

    // Press the button.
    let outcome = guest.control.call(Ctrl::Powerdown).expect("powerdown");
    assert!(
        matches!(outcome, Outcome::Ok { .. }),
        "powerdown must be accepted: {outcome:?}"
    );

    let event = guest
        .control
        .wait_event()
        .expect("control connection")
        .expect("shutdown event");
    assert!(
        matches!(
            event,
            Event::Shutdown {
                reason: ShutdownReason::GuestShutdown
            }
        ),
        "wanted guest shutdown after powerdown, got {event:?}"
    );
    let status = guest.wait_exit(Duration::from_secs(30));
    assert!(
        status.success(),
        "powerdown poweroff must exit 0: {status}\n{}",
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

    let dir = std::env::temp_dir().join(format!(
        "vmlab-hv-s1-missing-kernel-{}",
        std::process::id()
    ));
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
    assert!(
        !control.exists(),
        "no control socket may be left behind"
    );
    let _ = std::fs::remove_dir_all(&dir);
}
