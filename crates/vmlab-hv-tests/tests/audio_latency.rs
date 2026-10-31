//! A4 (hv-audio SPEC, "Testing decisions"): the loopback latency harness,
//! run against this host's real PipeWire session — outside the merge bar
//! (`#[ignore]`d; run with `cargo test -p vmlab-hv-tests --test
//! audio_latency -- --ignored -- --nocapture`).
//!
//! The machine plays a quiet click into a dedicated null sink
//! (`vmlab-a4`); the `monitor_tap` example captures that sink's monitor
//! stream and timestamps the click's arrival. Guest and host clocks are
//! related by an agent ping-pong: the guest prints its clock and the
//! harness times the receipt. The minimum receipt-to-print offset over
//! the rounds bounds the channel's one-way delay; using it can only
//! under-report latency by that bound, which the report prints.
//!
//! Reports guest-to-speaker latency at quantum 256 and at the default
//! quantum, against the 20 ms target (hv-audio D6).

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde_json::json;
use vmlab_hv_proto::client::Client;
use vmlab_hv_proto::machine::MachineDescription;
use vmlab_hv_tests::agent::AgentClient;
use vmlab_hv_tests::process::ManagedChild;

/// Amplitude of the click: about -30 dBFS, per "keep the volume low".
const CLICK_AMPLITUDE: i32 = 1000;
/// Silence before the click inside the guest's PCM file, subtracted
/// from every measurement.
const CLICK_LEAD_MS: u64 = 50;
const ROUNDS: usize = 8;

struct Vm {
    child: ManagedChild,
    dir: tempfile::TempDir,
}

impl Drop for Vm {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn host_unix_ns() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos()
}

fn target_dir() -> PathBuf {
    let dir = std::env::var_os("CARGO_TARGET_DIR").unwrap_or_else(|| "target".into());
    let dir = PathBuf::from(dir);
    if dir.is_absolute() {
        dir
    } else {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../..")
            .join(dir)
    }
}

/// Build `vmlab-hv` with the pipewire feature and the `monitor_tap`
/// example. Cargo is incremental, so repeated calls are cheap.
fn build_tools() -> Result<(), String> {
    let cargo = std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
    for args in [
        &["build", "-p", "vmlab-hv", "--features", "pipewire"][..],
        &[
            "build",
            "-p",
            "vmlab-hv-devices",
            "--features",
            "pipewire",
            "--example",
            "monitor_tap",
        ][..],
    ] {
        let status = Command::new(&cargo)
            .args(args)
            .current_dir(target_dir().parent().unwrap())
            .status()
            .map_err(|e| format!("spawn cargo: {e}"))
            .and_then(|status| {
                status
                    .success()
                    .then_some(())
                    .ok_or_else(|| format!("cargo {args:?} failed (PipeWire development headers?)"))
            });
        status?;
    }
    Ok(())
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

/// Boot the guest with its audio node routed to the harness's null sink.
/// `quantum` becomes `PIPEWIRE_QUANTUM` for the hypervisor process.
fn launch(quantum: Option<&str>) -> Vm {
    let firmware = Path::new("/usr/share/qemu/bios-256k.bin");
    let template = template().expect("template checked");
    let dir = tempfile::Builder::new().prefix("hv-a4-").tempdir().unwrap();
    let path = dir.path();
    let overlay = path.join("disk.qcow2");
    vmlab_disk::ops::create_linked_clone(&template, &overlay).unwrap();
    let description: MachineDescription = serde_json::from_value(json!({
        "name": "a4-guest", "machine": "q35", "arch": "x86_64", "accel": "kvm",
        "vcpus": 2, "memory_mib": 2048,
        "boot": {"kind": "firmware", "firmware": "seabios", "code": firmware,
            "bootorder": ["/pci@i0cf8/scsi@2/disk@0,0"]},
        "console_pci": {"bus": 0, "device": 1, "function": 0},
        "display": {"device": "std-vga", "pci": {"bus": 0, "device": 4, "function": 0}, "rom": null},
        "disks": [{"id": "disk0", "path": overlay, "format": "qcow2",
            "read_only": false, "vmstate": true,
            "pci": {"bus": 0, "device": 2, "function": 0}}],
        "nics": [], "serial_log": path.join("serial.log"),
        "ports": [{"name": "vmlab.agent.0", "socket": path.join("agent.sock")},
                  {"name": "vmlab.marker.0", "socket": path.join("marker.sock")}],
        "audio": {"device": "hda", "microphone": false,
            "node_name": "vmlab.a4", "node_description": "A4 loopback",
            "pci": {"bus": 0, "device": 27, "function": 0}}
    }))
    .unwrap();
    std::fs::write(
        path.join("machine.json"),
        serde_json::to_vec(&description).unwrap(),
    )
    .unwrap();
    let log = std::fs::File::create(path.join("hv.log")).unwrap();
    let mut command = Command::new(target_dir().join("debug/vmlab-hv"));
    // The session's default sink points at the harness's private null
    // sink for the run, so the machine's node lands there.
    command
        .arg("--machine")
        .arg(path.join("machine.json"))
        .arg("--control")
        .arg(path.join("control.sock"));
    if let Some(quantum) = quantum {
        command.env("PIPEWIRE_QUANTUM", quantum);
    }
    command
        .stdout(Stdio::from(log.try_clone().unwrap()))
        .stderr(Stdio::from(log));
    let mut child = ManagedChild::spawn(&mut command).expect("spawn vmlab-hv");
    let deadline = Instant::now() + Duration::from_secs(30);
    let control = loop {
        if let Ok(control) = Client::connect(path.join("control.sock")) {
            break control;
        }
        if Instant::now() >= deadline || child.try_wait().unwrap().is_some() {
            panic!(
                "control socket never appeared: {}",
                std::fs::read_to_string(path.join("hv.log")).unwrap_or_default()
            );
        }
        std::thread::sleep(Duration::from_millis(25));
    };
    drop(control);
    Vm { child, dir }
}

/// The monitor tap, logging CLICK lines to a file.
struct Tap {
    child: ManagedChild,
    log: PathBuf,
}

impl Drop for Tap {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Tap {
    fn start(target: &str, seconds: u64) -> Tap {
        let log = std::env::temp_dir().join(format!("vmlab-a4-tap-{}.log", std::process::id()));
        let file = std::fs::File::create(&log).unwrap();
        let mut command = Command::new(target_dir().join("debug/examples/monitor_tap"));
        command
            .args([
                target,
                &seconds.to_string(),
                &(CLICK_AMPLITUDE / 2).to_string(),
            ])
            .stdout(Stdio::from(file));
        let child = ManagedChild::spawn(&mut command).expect("spawn monitor_tap");
        let tap = Tap { child, log };
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            if std::fs::read_to_string(&tap.log)
                .map(|text| text.contains("TAP-READY"))
                .unwrap_or(false)
            {
                return tap;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        panic!(
            "monitor_tap never became ready: {}",
            std::fs::read_to_string(&tap.log).unwrap_or_default()
        );
    }

    /// Every CLICK timestamp, in unix nanoseconds, in order.
    fn clicks_ns(&self) -> Vec<u128> {
        std::fs::read_to_string(&self.log)
            .unwrap_or_default()
            .lines()
            .filter_map(|line| line.strip_prefix("CLICK "))
            .filter_map(|rest| rest.split(' ').next())
            .filter_map(|ns| ns.parse::<u128>().ok())
            .collect()
    }
}

/// The guest-side device node of the marker virtio-serial port.
fn marker_device(agent: &mut AgentClient) -> String {
    let found = agent
        .exec(
            &[
                "sh",
                "-ec",
                "ls /dev/virtio-ports/vmlab.marker.0 2>/dev/null || ls /dev/vport0p2",
            ],
            Duration::from_secs(60),
        )
        .unwrap();
    String::from_utf8_lossy(&found.stdout).trim().to_string()
}

#[test]
#[ignore = "A4: the loopback harness runs against the live PipeWire session"]
fn loopback_latency_against_real_pipewire() {
    let firmware = Path::new("/usr/share/qemu/bios-256k.bin");
    if !Path::new("/dev/kvm").exists() || !firmware.is_file() || template().is_none() {
        eprintln!("SKIP: A4 requires /dev/kvm, SeaBIOS and the ubuntu-26.04 template");
        return;
    }
    let pipewire_up = std::env::var_os("XDG_RUNTIME_DIR").is_some_and(|dir| {
        std::os::unix::net::UnixStream::connect(PathBuf::from(dir).join("pipewire-0")).is_ok()
    });
    if !pipewire_up {
        eprintln!("SKIP: A4 requires a reachable PipeWire session");
        return;
    }
    if which("pactl").is_none() {
        eprintln!("SKIP: A4 requires pactl to create the null sink");
        return;
    }
    if let Err(e) = build_tools() {
        eprintln!("SKIP: A4 tools did not build: {e}");
        return;
    }

    // A private sink for the machine, tapped through its monitor. The
    // name carries the harness's pid: a leftover sink from a panicked
    // earlier run must not make the target ambiguous, or the tap's
    // AUTOCONNECT silently captures the desktop's default source.
    let sink = format!("vmlab-a4-{}", std::process::id());
    let module = Command::new("pactl")
        .args([
            "load-module",
            "module-null-sink",
            &format!("sink_name={sink}"),
        ])
        .output()
        .expect("pactl");
    assert!(
        module.status.success(),
        "creating the null sink: {}",
        String::from_utf8_lossy(&module.stderr)
    );
    let module_id = String::from_utf8_lossy(&module.stdout).trim().to_string();

    // `target.object` does not resolve reliably between libpipewire and
    // this crate, and an unresolved target silently AUTOCONNECTS to the
    // desktop's default source (a microphone). For the harness's
    // duration, point the session's default sink and source at the
    // private sink and its monitor, and restore both afterwards; then
    // every fallback path also lands on the tap.
    let defaults = Defaults::capture();
    defaults.apply(&sink);

    for (label, quantum) in [
        ("quantum 256", Some("256/48000")),
        ("default quantum", None),
    ] {
        let tap = Tap::start(&sink, 300);
        let vm = launch(quantum);
        let mut agent =
            AgentClient::connect(&vm.dir.path().join("agent.sock"), Duration::from_secs(120))
                .unwrap_or_else(|e| panic!("{e}"));
        let script = include_str!("audio_alsa.py").replace('\'', "'\\''");
        agent
            .exec(
                &[
                    "sh",
                    "-ec",
                    &format!("printf '%s' '{script}' > /tmp/alsa.py"),
                ],
                Duration::from_secs(60),
            )
            .unwrap();
        agent
            .exec(
                &["python3", "/tmp/alsa.py", "unmute"],
                Duration::from_secs(60),
            )
            .unwrap();
        // The click: 50 ms of silence, 3 ms of decaying impulse, then
        // silence to drain the pipe.
        let click = "python3 - <<'EOF'
import struct
frames = bytearray()
frames += bytes(4 * 48000 * 50 // 1000)
for t in range(48 * 3):
    s = int(CLICK * (1.0 - t / 144.0))
    frames += struct.pack('<hh', s, s)
frames += bytes(4 * 48000)
open('/tmp/click.raw', 'wb').write(frames)
EOF"
        .replace("CLICK", &CLICK_AMPLITUDE.to_string());
        agent
            .exec(&["sh", "-ec", &click], Duration::from_secs(60))
            .unwrap();

        // The marker port carries the play instant out of the guest on
        // virtio-serial, so every timestamp below is a host timestamp.
        let marker = vm.dir.path().join("marker.sock");
        let marker_stream =
            std::os::unix::net::UnixStream::connect(&marker).expect("connect the marker port");
        marker_stream
            .set_read_timeout(Some(Duration::from_millis(20)))
            .unwrap();
        let markers = std::sync::Arc::new(parking_lot::Mutex::new(Vec::new()));
        let sink = markers.clone();
        let reader = std::thread::spawn(move || {
            use std::io::Read;
            let mut stream = marker_stream;
            let mut buffer = [0u8; 64];
            loop {
                match stream.read(&mut buffer) {
                    Ok(0) => break,
                    Ok(n) => {
                        let at = host_unix_ns();
                        // One timestamp per marker byte: reads can carry
                        // more than one round's marker.
                        let count = buffer[..n].iter().filter(|b| **b == b'M').count();
                        for _ in 0..count {
                            sink.lock().push(at);
                        }
                    }
                    Err(e)
                        if e.kind() == std::io::ErrorKind::WouldBlock
                            || e.kind() == std::io::ErrorKind::TimedOut => {}
                    Err(_) => break,
                }
            }
        });
        let marker_device = marker_device(&mut agent);
        let mut rounds: Vec<(u128, Option<u128>)> = Vec::new();
        let mut clicks_seen = 0;
        for _ in 0..ROUNDS {
            let markers_before = markers.lock().len();
            let play = agent
                .exec(
                    &[
                        "sh",
                        "-ec",
                        &format!(
                            "ALSA_MARKER={marker_device} \
                             python3 /tmp/alsa.py play 48000 2 < /tmp/click.raw"
                        ),
                    ],
                    Duration::from_secs(60),
                )
                .unwrap();
            assert_eq!(play.exit_code, 0);
            // Wait a moment, then pair this round's marker and click.
            std::thread::sleep(Duration::from_millis(500));
            let marker_ns = markers.lock().get(markers_before).copied();
            let clicks = tap.clicks_ns();
            let click = if clicks.len() > clicks_seen {
                clicks_seen = clicks.len();
                clicks.last().copied()
            } else {
                None
            };
            rounds.push((marker_ns.expect("the marker never crossed the port"), click));
        }
        let mut measured = Vec::new();
        for (played_at, click) in &rounds {
            let Some(click) = click else { continue };
            let latency = click
                .saturating_sub(*played_at)
                .saturating_sub(u128::from(CLICK_LEAD_MS) * 1_000_000);
            measured.push(latency as f64 / 1_000_000.0);
        }
        drop(reader);
        measured.sort_by(|a, b| a.partial_cmp(b).unwrap());
        if measured.is_empty() {
            eprintln!("A4 [{label}]: the monitor tap never saw a click");
        } else {
            println!(
                "A4 [{label}]: guest-to-speaker latency min {:.1} ms, median {:.1} ms over {} clicks (target: 20 ms)",
                measured[0],
                measured[measured.len() / 2],
                measured.len()
            );
            assert!(
                measured[measured.len() / 2] < 200.0,
                "the loopback path is not merely slow but broken: {measured:?}"
            );
        }
        drop(vm);
        drop(tap);
    }

    drop(defaults);
    let unload = Command::new("pactl")
        .args(["unload-module", &module_id])
        .output()
        .expect("pactl");
    assert!(unload.status.success(), "unloading the null sink failed");
}

/// The session's default sink and source, saved and restored around the
/// harness run.
struct Defaults {
    sink: Option<String>,
    source: Option<String>,
}

impl Defaults {
    fn capture() -> Self {
        Defaults {
            sink: pactl("get-default-sink"),
            source: pactl("get-default-source"),
        }
    }

    fn apply(&self, sink: &str) {
        let _ = Command::new("pactl")
            .args(["set-default-sink", sink])
            .output();
        let _ = Command::new("pactl")
            .args(["set-default-source", &format!("{sink}.monitor")])
            .output();
    }

    fn restore(&self) {
        if let Some(sink) = &self.sink {
            let _ = Command::new("pactl")
                .args(["set-default-sink", sink])
                .output();
        }
        if let Some(source) = &self.source {
            let _ = Command::new("pactl")
                .args(["set-default-source", source])
                .output();
        }
    }
}

impl Drop for Defaults {
    fn drop(&mut self) {
        self.restore();
    }
}

fn pactl(query: &str) -> Option<String> {
    let output = Command::new("pactl").args([query]).output().ok()?;
    let text = String::from_utf8_lossy(&output.stdout).trim().to_string();
    (!text.is_empty()).then_some(text)
}

fn which(program: &str) -> Option<PathBuf> {
    std::env::var_os("PATH").and_then(|paths| {
        std::env::split_paths(&paths)
            .map(|dir| dir.join(program))
            .find(|path| path.is_file())
    })
}
