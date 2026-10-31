//! A3 (hv-audio SPEC, "Testing decisions"): a real Linux guest with audio
//! on, observed through the file-backed recording host. The binary is
//! built with the `testing` feature and `VMLAB_HV_TEST_AUDIO_PLAYBACK`
//! set, so the machine's HDA plays into a dump file instead of a sound
//! server and `VMLAB_HV_TEST_AUDIO_CAPTURE` scripts what it records.
//!
//! The ubuntu-26.04 template carries snd_hda_intel but no alsa-utils, so
//! the guest is driven through `audio_alsa.py` (the public ALSA kernel
//! interface: PCM ioctls for aplay/arecord, control ioctls to set the
//! amplifiers the way a desktop session would). Skips with a reason
//! without `/dev/kvm`, SeaBIOS or the template.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::json;
use vmlab_hv_proto::client::Client;
use vmlab_hv_proto::control::{Command as Ctrl, Outcome};
use vmlab_hv_proto::machine::MachineDescription;
use vmlab_hv_tests::agent::{AgentClient, ExecOutput};
use vmlab_hv_tests::process::ManagedChild;

/// One audio guest: the process, its directory and its control client.
struct AudioVm {
    child: ManagedChild,
    dir: tempfile::TempDir,
    control: Client,
}

impl Drop for AudioVm {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl AudioVm {
    fn diagnostics(&self) -> String {
        let log = std::fs::read_to_string(self.dir.path().join("hv.log")).unwrap_or_default();
        let serial =
            std::fs::read_to_string(self.dir.path().join("serial.log")).unwrap_or_default();
        let tail: String = serial
            .chars()
            .skip(serial.len().saturating_sub(4000))
            .collect();
        format!("--- hv.log ---\n{log}\n--- serial tail ---\n{tail}")
    }

    fn agent(&self) -> AgentClient {
        AgentClient::connect(
            &self.dir.path().join("agent.sock"),
            Duration::from_secs(120),
        )
        .unwrap_or_else(|e| panic!("{e}\n{}", self.diagnostics()))
    }

    fn call(&mut self, command: Ctrl) {
        let reply = self.control.call(command).unwrap();
        assert!(
            matches!(reply, Outcome::Ok { .. }),
            "{reply:?}\n{}",
            self.diagnostics()
        );
    }

    fn exec(&self, agent: &mut AgentClient, script: &str) -> ExecOutput {
        self.run(agent, &["sh", "-ec", script])
    }

    fn run(&self, agent: &mut AgentClient, argv: &[&str]) -> ExecOutput {
        let output = agent
            .exec(argv, Duration::from_secs(240))
            .unwrap_or_else(|e| panic!("`{}`: {e}\n{}", argv.join(" "), self.diagnostics()));
        assert_eq!(
            output.exit_code,
            0,
            "{}: stdout={} stderr={}\n{}",
            argv[0],
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
            self.diagnostics()
        );
        output
    }

    fn dump(&self) -> Vec<u8> {
        std::fs::read(self.dir.path().join("playback.pcm")).unwrap_or_default()
    }

    fn events(&self) -> String {
        std::fs::read_to_string(self.dir.path().join("playback.pcm.events")).unwrap_or_default()
    }
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

/// Boot one audio guest. `microphone` selects the machine description's
/// audio section; the capture script is written before the machine starts,
/// because the recording host reads it when the guest opens its capture
/// stream.
fn launch(microphone: bool) -> Option<AudioVm> {
    let firmware = Path::new("/usr/share/qemu/bios-256k.bin");
    if !Path::new("/dev/kvm").exists()
        || !firmware.is_file()
        || !Path::new("/usr/share/qemu/vgabios-stdvga.bin").is_file()
    {
        eprintln!("SKIP: guest audio requires /dev/kvm, SeaBIOS and SeaVGABIOS");
        return None;
    }
    let Some(template) = template() else {
        eprintln!("SKIP: no ubuntu-26.04 template; set VMLAB_HV_TEST_VM_DISK");
        return None;
    };
    let dir = tempfile::Builder::new()
        .prefix("hv-audio-guest-")
        .tempdir()
        .unwrap();
    let path = dir.path();
    let overlay = path.join("disk.qcow2");
    vmlab_disk::ops::create_linked_clone(&template, &overlay).unwrap();
    // One second of a marker pattern — frames of (0x1357, -0x2468) — that
    // the guest's recording must contain verbatim: the capture amplifiers
    // sit at 0 dB, so nothing may disturb the bytes.
    let mut script = Vec::with_capacity(48000 * 4);
    for _ in 0..48000 {
        script.extend_from_slice(&0x1357i16.to_le_bytes());
        script.extend_from_slice(&(-0x2468i16).to_le_bytes());
    }
    std::fs::write(path.join("capture-script.pcm"), script).unwrap();
    let playback = path.join("playback.pcm");
    let description: MachineDescription = serde_json::from_value(json!({
        "name": "audio-guest", "machine": "q35", "arch": "x86_64", "accel": "kvm",
        "vcpus": 2, "memory_mib": 2048,
        "boot": {"kind": "firmware", "firmware": "seabios", "code": firmware,
            "bootorder": ["/pci@i0cf8/scsi@2/disk@0,0"]},
        "console_pci": {"bus": 0, "device": 1, "function": 0},
        "display": {"device": "std-vga", "pci": {"bus": 0, "device": 4, "function": 0}, "rom": null},
        "disks": [{"id": "disk0", "path": overlay, "format": "qcow2",
            "read_only": false, "vmstate": true,
            "pci": {"bus": 0, "device": 2, "function": 0}}],
        "nics": [], "serial_log": path.join("serial.log"),
        "ports": [{"name": "vmlab.agent.0", "socket": path.join("agent.sock")}],
        "audio": {"device": "hda", "microphone": microphone,
            "node_name": "vmlab.test.audio", "node_description": "A3 guest",
            "pci": {"bus": 0, "device": 27, "function": 0}}
    }))
    .unwrap();
    std::fs::write(
        path.join("machine.json"),
        serde_json::to_vec(&description).unwrap(),
    )
    .unwrap();
    let log = std::fs::File::create(path.join("hv.log")).unwrap();
    let mut child = ManagedChild::spawn(
        Command::new(vmlab_hv_tests::hv_binary_with(&["testing"]))
            .arg("--machine")
            .arg(path.join("machine.json"))
            .arg("--control")
            .arg(path.join("control.sock"))
            .env("VMLAB_HV_TEST_AUDIO_PLAYBACK", &playback)
            .env(
                "VMLAB_HV_TEST_AUDIO_CAPTURE",
                path.join("capture-script.pcm"),
            )
            .stdout(Stdio::from(log.try_clone().unwrap()))
            .stderr(Stdio::from(log)),
    )
    .unwrap();
    let deadline = Instant::now() + Duration::from_secs(30);
    let control = loop {
        if let Ok(control) = Client::connect(path.join("control.sock")) {
            break control;
        }
        if Instant::now() >= deadline || child.try_wait().unwrap().is_some() {
            let _ = child.kill();
            let _ = child.wait();
            panic!(
                "control unavailable: {}",
                std::fs::read_to_string(path.join("hv.log")).unwrap()
            );
        }
        std::thread::sleep(Duration::from_millis(25));
    };
    Some(AudioVm {
        child,
        dir,
        control,
    })
}

/// The guest-side client, pushed as one quoted blob.
fn install_client(vm: &AudioVm, agent: &mut AgentClient) {
    let script = include_str!("audio_alsa.py").replace('\'', "'\\''");
    vm.exec(agent, &format!("printf '%s' '{script}' > /tmp/alsa.py"));
}

/// Write a tone of `hz` for `seconds` guest-side.
fn write_tone(vm: &AudioVm, agent: &mut AgentClient, hz: u32, seconds: u32) {
    vm.exec(
        agent,
        &format!(
            "python3 - <<'EOF'
import math, struct
frames = bytearray()
for t in range(48000 * {seconds}):
    s = int(6000 * math.sin(2 * math.pi * {hz} * t / 48000))
    frames += struct.pack('<hh', s, s)
open('/tmp/tone.raw', 'wb').write(frames)
EOF"
        ),
    );
}

/// Wait until the dump holds at least `seconds` of frames past `from`.
fn wait_for_dump(vm: &AudioVm, from: usize, seconds: u32) -> Vec<u8> {
    let want = from + (seconds as usize) * 48000 * 4;
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let dump = vm.dump();
        if dump.len() >= want || Instant::now() >= deadline {
            assert!(
                dump.len() >= want,
                "only {} of {want} PCM bytes reached the recording host\n{}",
                dump.len(),
                vm.diagnostics()
            );
            return dump;
        }
        std::thread::sleep(Duration::from_millis(200));
    }
}

/// One frame of the dump's left channel.
fn frame(pcm: &[u8], index: usize) -> i16 {
    i16::from_le_bytes([pcm[index * 4], pcm[index * 4 + 1]])
}

/// The dominant frequency of the dump's left channel over the frames in
/// `range`, by upward zero crossings — precise to a few hertz over a
/// half second at 48 kHz, and blind to phase and gain.
fn dominant_frequency(pcm: &[u8], range: std::ops::Range<usize>) -> f64 {
    let crossings = range
        .clone()
        .filter(|i| frame(pcm, *i) <= 0 && frame(pcm, i + 1) > 0)
        .count() as f64;
    crossings * 48000.0 / range.len() as f64
}

/// The root-mean-square of the same window, in samples.
fn window_rms(pcm: &[u8], range: std::ops::Range<usize>) -> f64 {
    let sum: i64 = range
        .clone()
        .map(|i| i64::from(i32::from(frame(pcm, i)).pow(2)))
        .sum();
    (sum as f64 / range.len() as f64).sqrt()
}

#[test]
fn guest_audio_plays_records_and_survives_snapshots() {
    let Some(mut vm) = launch(true) else {
        return;
    };
    let mut agent = vm.agent();
    install_client(&vm, &mut agent);
    vm.exec(&mut agent, "python3 /tmp/alsa.py unmute");

    // --- aplay: a known tone file lands intact -------------------------
    write_tone(&vm, &mut agent, 440, 2);
    vm.exec(
        &mut agent,
        "python3 /tmp/alsa.py play 48000 2 < /tmp/tone.raw",
    );
    let dump = wait_for_dump(&vm, 0, 2);
    assert!(
        (430.0..=450.0).contains(&dominant_frequency(&dump, 48000..96000)),
        "the recording host did not receive the 440 Hz tone"
    );
    assert!(
        window_rms(&dump, 48000..96000) > 2000.0,
        "the tone arrived attenuated"
    );
    assert!(
        vm.events()
            .lines()
            .any(|line| line == "playback 48000 2 s16le"),
        "stream events: {}",
        vm.events()
    );

    // --- arecord: the scripted source reaches the guest ----------------
    let recorded = vm.run(
        &mut agent,
        &[
            "sh",
            "-ec",
            "python3 /tmp/alsa.py record 48000 2 2 > /tmp/rec.raw && base64 -w0 /tmp/rec.raw",
        ],
    );
    let raw = base64_decode(&recorded.stdout);
    let marker: Vec<u8> = (0..64)
        .flat_map(|_| [0x1357i16.to_le_bytes(), (-0x2468i16).to_le_bytes()].concat())
        .collect();
    assert!(
        raw.windows(marker.len()).any(|w| w == marker),
        "the guest's recording does not contain the scripted source"
    );
    assert!(
        vm.events().lines().any(|line| line.starts_with("capture ")),
        "no capture stream was opened with the microphone on"
    );

    // --- an online snapshot mid-playback restores with audio resuming --
    write_tone(&vm, &mut agent, 880, 6);
    vm.exec(
        &mut agent,
        "python3 /tmp/alsa.py play 48000 2 < /tmp/tone.raw >/dev/null 2>&1 &",
    );
    std::thread::sleep(Duration::from_millis(1500));
    vm.call(Ctrl::SnapshotSave {
        name: "audio".into(),
    });
    // The machine resumed; let the original play run out.
    std::thread::sleep(Duration::from_secs(7));
    let before = vm.dump().len();
    assert!(before > 0);
    vm.call(Ctrl::SnapshotLoad {
        name: "audio".into(),
    });
    vm.call(Ctrl::Resume);
    // Restored into the middle of the 880 Hz play: audio resumes and the
    // dump keeps growing with the same tone.
    let dump = wait_for_dump(&vm, before, 2);
    assert!(
        (870.0..=890.0).contains(&dominant_frequency(
            &dump,
            before / 4 + 48000..before / 4 + 48000 + 48000
        )),
        "audio did not resume after the snapshot round trip"
    );
    vm.exec(&mut agent, "sync");
}

#[test]
fn microphone_off_opens_no_capture_stream() {
    let Some(vm) = launch(false) else {
        return;
    };
    let mut agent = vm.agent();
    install_client(&vm, &mut agent);
    vm.exec(&mut agent, "python3 /tmp/alsa.py unmute");
    // The capture device records — silence, because the controller DMA
    // writes zeros. What must NOT exist is a host capture stream.
    vm.exec(
        &mut agent,
        "python3 /tmp/alsa.py record 48000 2 1 > /tmp/rec.raw || true; sleep 1",
    );
    assert!(
        !vm.events().lines().any(|line| line.starts_with("capture ")),
        "a capture stream opened with the microphone off: {}",
        vm.events()
    );
}

/// Standard base64 with padding, as `base64 -w0` emits.
fn base64_decode(text: &[u8]) -> Vec<u8> {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = Vec::with_capacity(text.len() / 4 * 3);
    let mut buffer = 0u32;
    let mut bits = 0u32;
    for &byte in text {
        let Some(value) = TABLE.iter().position(|t| *t == byte) else {
            continue;
        };
        buffer = (buffer << 6) | value as u32;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((buffer >> bits) as u8);
        }
    }
    out
}
