//! S1: daemon-owned TAP queues installed over the paused control socket.
//! Run in a private network namespace with VMLAB_HV_TEST_TAP=1.

use std::fs::OpenOptions;
use std::io;
use std::net::UdpSocket;
use std::os::fd::{AsFd, AsRawFd, OwnedFd};
use std::os::unix::fs::OpenOptionsExt;
use std::process::Command;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use vmlab_cinit_proto::CtlEvent;
use vmlab_hv_proto::control::{Capability, Command as Ctrl, ErrorCode, Outcome};
use vmlab_hv_tests::agent::AgentClient;
use vmlab_hv_tests::ctl::CtlClient;
use vmlab_hv_tests::{MachineSpec, TEST_MAC, mac_string, probe_assets};

const BUSYBOX: &str = "/.vmlab/busybox";

fn tap_queues(pairs: u16) -> io::Result<(String, Vec<OwnedFd>)> {
    let mut name = "ngs%d".to_string();
    let mut fds = Vec::new();
    for _ in 0..pairs {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .custom_flags(libc::O_NONBLOCK | libc::O_CLOEXEC)
            .open("/dev/net/tun")?;
        // SAFETY: a zero-initialized UAPI ifreq has valid integer/name storage.
        let mut request: libc::ifreq = unsafe { std::mem::zeroed() };
        for (target, byte) in request.ifr_name.iter_mut().zip(name.bytes()) {
            *target = byte as libc::c_char;
        }
        request.ifr_ifru.ifru_flags =
            (libc::IFF_TAP | libc::IFF_NO_PI | libc::IFF_VNET_HDR | libc::IFF_MULTI_QUEUE)
                as libc::c_short;
        // SAFETY: the fd is owned, and TUNSETIFF reads/writes this live ifreq.
        if unsafe { libc::ioctl(file.as_raw_fd(), 0x4004_54ca as libc::c_ulong, &mut request) } < 0
        {
            return Err(io::Error::last_os_error());
        }
        name = request
            .ifr_name
            .iter()
            .take_while(|byte| **byte != 0)
            .map(|byte| *byte as u8 as char)
            .collect();
        fds.push(file.into());
    }
    Ok((name, fds))
}

fn ip(args: &[&str]) {
    let output = Command::new("ip")
        .args(args)
        .output()
        .expect("ip tool for TAP test");
    assert!(
        output.status.success(),
        "ip {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

struct Echo {
    stop: Arc<AtomicBool>,
    worker: Option<std::thread::JoinHandle<()>>,
}

impl Echo {
    fn start() -> Self {
        let socket = UdpSocket::bind("192.0.2.1:46000").unwrap();
        socket
            .set_read_timeout(Some(Duration::from_millis(100)))
            .unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let done = stop.clone();
        let worker = std::thread::spawn(move || {
            let mut buffer = [0; 65536];
            while !done.load(Ordering::Acquire) {
                match socket.recv_from(&mut buffer) {
                    Ok((count, peer)) => {
                        socket.send_to(&buffer[..count], peer).unwrap();
                    }
                    Err(error)
                        if matches!(
                            error.kind(),
                            io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
                        ) => {}
                    Err(error) => panic!("TAP echo: {error}"),
                }
            }
        });
        Self {
            stop,
            worker: Some(worker),
        }
    }
}

impl Drop for Echo {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        self.worker.take().unwrap().join().unwrap();
    }
}

#[test]
fn passed_tap_and_vhost_queues_exchange_traffic_and_restore() {
    if std::env::var("VMLAB_HV_TEST_TAP").as_deref() != Ok("1") {
        eprintln!("skip TAP S1: set VMLAB_HV_TEST_TAP=1 in a private network namespace");
        return;
    }
    let assets = probe_assets();
    if let Some(reason) = assets.skip_reason() {
        eprintln!("skip TAP S1: {reason}");
        return;
    }
    for (vhost, raw) in [(false, false), (true, false), (true, true)] {
        let (name, mut fds) = match tap_queues(2) {
            Ok(result) => result,
            Err(error) if matches!(error.raw_os_error(), Some(libc::EPERM | libc::EACCES)) => {
                eprintln!("skip TAP S1: creating test-owned TAP needs CAP_NET_ADMIN: {error}");
                return;
            }
            Err(error) => panic!("create test TAP: {error}"),
        };
        ip(&["addr", "add", "192.0.2.1/24", "dev", &name]);
        ip(&["link", "set", "dev", &name, "mtu", "1500", "up"]);
        if vhost {
            for _ in 0..2 {
                match OpenOptions::new()
                    .read(true)
                    .write(true)
                    .open("/dev/vhost-net")
                {
                    Ok(file) => fds.push(file.into()),
                    Err(error) => {
                        eprintln!("skip vhost TAP S1: /dev/vhost-net unavailable: {error}");
                        return;
                    }
                }
            }
        }
        let mut spec =
            MachineSpec::idle(if vhost { "s1-vhost-net" } else { "s1-tap-net" }).with_vnet_pairs(2);
        spec.tap = true;
        spec.tap_raw = raw;
        spec.paused = true;
        spec.vcpus = 2;
        let mut guest = vmlab_hv_tests::launch(&spec, &assets);
        assert!(guest.control.has_capability(Capability::NetFds));
        assert!(matches!(
            guest.control.call(Ctrl::Resume).unwrap(),
            Outcome::Error {
                code: ErrorCode::InvalidState,
                ..
            }
        ));
        let command = Ctrl::NetAttach {
            mac: mac_string(TEST_MAC),
            queue_pairs: 2,
            vhost,
        };
        let borrowed: Vec<_> = fds.iter().map(AsFd::as_fd).collect();
        assert!(matches!(
            guest
                .control
                .call_with_fds(command.clone(), &borrowed)
                .unwrap(),
            Outcome::Ok { .. }
        ));
        assert!(matches!(
            guest.control.call_with_fds(command, &borrowed).unwrap(),
            Outcome::Error {
                code: ErrorCode::InvalidArgument,
                ..
            }
        ));
        drop(borrowed);
        drop(fds);
        assert!(matches!(
            guest.control.call(Ctrl::Resume).unwrap(),
            Outcome::Ok { .. }
        ));
        let mut container = spec.container_spec();
        container.nics = 0;
        let mut ctl = CtlClient::connect(&guest.ctl_sock, Duration::from_secs(10)).unwrap();
        ctl.send_spec(&container).unwrap();
        assert!(matches!(
            ctl.wait_event(
                &container,
                Duration::from_secs(120),
                |event| matches!(event, CtlEvent::Idle),
                |_| {},
                || guest.diagnostics("TAP boot")
            ),
            CtlEvent::Idle
        ));
        let mut agent = AgentClient::connect(&guest.agent_sock, Duration::from_secs(60)).unwrap();
        if raw {
            let output = agent.exec(&[BUSYBOX, "sh", "-c", "for d in /sys/bus/virtio/devices/*; do k=$(/.vmlab/busybox cat \"$d/device\"); [ \"$((k))\" -eq 1 ] && /.vmlab/busybox cat \"$d/features\"; done"], Duration::from_secs(15)).unwrap();
            for bit in [0, 1, 7, 8, 9, 11, 12, 13, 54, 55, 56] {
                assert_eq!(
                    output.stdout.get(bit),
                    Some(&b'0'),
                    "raw TAP offload bit {bit}: {output:?}"
                );
            }
        }
        let output = agent.exec(&[BUSYBOX, "sh", "-c", "/.vmlab/busybox ip link set dev eth0 up && /.vmlab/busybox ip addr add 192.0.2.2/24 dev eth0; set -- /sys/class/net/eth0/queues/rx-*; [ \"$#\" -eq 2 ]"], Duration::from_secs(15)).unwrap();
        assert_eq!(output.exit_code, 0, "TAP guest configuration: {output:?}");
        let echo = Echo::start();
        let traffic = |agent: &mut AgentClient, guest: &vmlab_hv_tests::Guest, phase: &str| {
            let output = agent
                .exec(
                    &[
                        BUSYBOX,
                        "ping",
                        "-q",
                        "-c",
                        "20",
                        "-i",
                        "0.01",
                        "-W",
                        "2",
                        "192.0.2.1",
                    ],
                    Duration::from_secs(30),
                )
                .unwrap();
            assert_eq!(
                output.exit_code,
                0,
                "{vhost}: {phase}: ping: {output:?}\n{}",
                guest.diagnostics(phase)
            );
            let command = format!(
                "for flow in 0 1 2 3 4 5 6 7 8 9 10 11 12 13 14 15; do (printf 'tap-%s {phase}\\n' \"$flow\" | /.vmlab/busybox nc -u -w 1 192.0.2.1 46000) & done; wait"
            );
            let output = agent
                .exec(&[BUSYBOX, "sh", "-c", &command], Duration::from_secs(15))
                .unwrap();
            assert_eq!(output.exit_code, 0, "{vhost}: {phase}: UDP: {output:?}");
            let mut actual: Vec<_> = String::from_utf8(output.stdout)
                .unwrap()
                .lines()
                .map(str::to_string)
                .collect();
            actual.sort();
            let mut expected: Vec<_> = (0..16).map(|flow| format!("tap-{flow} {phase}")).collect();
            expected.sort();
            assert_eq!(actual, expected, "{vhost}: {phase}: UDP payloads");
        };
        traffic(&mut agent, &guest, "before save");
        assert!(matches!(
            guest.control.call(Ctrl::Pause).unwrap(),
            Outcome::Ok { .. }
        ));
        assert!(matches!(
            guest
                .control
                .call(Ctrl::SnapshotSave { name: "tap".into() })
                .unwrap(),
            Outcome::Ok { .. }
        ));
        let resumed = guest.control.call(Ctrl::Resume).unwrap();
        assert!(
            matches!(resumed, Outcome::Ok { .. }),
            "TAP resume after paused snapshot: {resumed:?}; backend diagnostic: {:?}",
            guest.control.call(Ctrl::SnapshotSave {
                name: "resume-diagnostic".into()
            })
        );
        traffic(&mut agent, &guest, "after save");
        assert!(matches!(
            guest
                .control
                .call(Ctrl::SnapshotLoad { name: "tap".into() })
                .unwrap(),
            Outcome::Ok { .. }
        ));
        assert!(matches!(
            guest.control.call(Ctrl::Resume).unwrap(),
            Outcome::Ok { .. }
        ));
        drop(agent);
        let mut agent = AgentClient::connect(&guest.agent_sock, Duration::from_secs(60)).unwrap();
        traffic(&mut agent, &guest, "after load");
        drop(echo);
        guest.quit_and_exit_zero();
    }
}
