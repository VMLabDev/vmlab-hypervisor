//! Windows inbox HDA driver acceptance through S1 on a disposable overlay.
//!
//! By default, checks PCM with the recording backend. Set
//! VMLAB_HV_TEST_AUDIO_PIPEWIRE to an absolute PipeWire socket path to check a
//! running host node instead. The tone is three seconds at less than 1% amplitude.
//! The agent uses exclusive WASAPI to exercise the inbox driver directly.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use vmlab_hv_proto::{
    client::Client,
    control::{Command as Control, Outcome},
};
use vmlab_hv_tests::{agent::AgentClient, process::ManagedChild};

fn template() -> Option<PathBuf> {
    if let Some(path) = std::env::var_os("VMLAB_HV_TEST_WINDOWS_DISK") {
        let path = PathBuf::from(path);
        assert!(
            path.is_file(),
            "Windows template missing: {}",
            path.display()
        );
        return Some(path);
    }
    let root = PathBuf::from(std::env::var_os("HOME")?)
        .join(".local/share/vmlab/templates/x86_64/windows-11");
    let mut disks: Vec<_> = std::fs::read_dir(root)
        .ok()?
        .filter_map(Result::ok)
        .map(|entry| entry.path().join("variants/standard/disk.qcow2"))
        .filter(|path| path.is_file())
        .collect();
    disks.sort();
    disks.pop()
}

fn powershell(agent: &mut AgentClient, script: &str) -> Vec<u8> {
    let output = agent
        .exec(
            &["powershell.exe", "-NoProfile", "-Command", script],
            Duration::from_secs(90),
        )
        .unwrap();
    assert_eq!(
        output.exit_code,
        0,
        "Windows: {}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    if !output.stdout.is_empty() {
        eprintln!("Windows: {}", String::from_utf8_lossy(&output.stdout));
    }
    output.stdout
}

struct Vm {
    child: ManagedChild,
    dir: tempfile::TempDir,
}

impl Drop for Vm {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        if std::thread::panicking() {
            eprintln!(
                "Windows HDA log:\n{}",
                std::fs::read_to_string(self.dir.path().join("hv.log")).unwrap_or_default()
            );
        }
    }
}

fn pipewire_node(remote: &Path) -> Option<Value> {
    let output = Command::new("pw-dump")
        .arg("--remote")
        .arg(remote)
        .output()
        .expect("run pw-dump");
    assert!(output.status.success(), "pw-dump: {output:?}");
    let objects: Vec<Value> = serde_json::from_slice(&output.stdout).unwrap();
    objects.into_iter().find(|object| {
        object["info"]["props"]["node.name"] == "vmlab.test.windows-audio"
            && object["info"]["state"] == "running"
    })
}

#[test]
#[cfg_attr(vmlab_ci_no_kvm, ignore = "requires KVM and/or local guest assets")]
fn windows_inbox_hda_starts_and_plays_via_s1() {
    if !Path::new("/dev/kvm").exists() {
        eprintln!("SKIP Windows HDA S1: /dev/kvm unavailable");
        return;
    }
    let Some(template) = template() else {
        eprintln!("SKIP Windows HDA S1: Windows 11 standard template unavailable");
        return;
    };
    let firmware = std::env::var_os("VMLAB_HV_FIRMWARE_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            PathBuf::from(std::env::var_os("HOME").unwrap())
                .join(".local/share/vmlab/guest/firmware/x86_64")
        });
    let code = firmware.join("OVMF_CODE_4M.secboot.fd");
    let vars = firmware.join("OVMF_VARS_4M.ms.fd");
    for path in [&code, &vars] {
        if !path.is_file() {
            eprintln!("SKIP Windows HDA S1: {} unavailable", path.display());
            return;
        }
    }
    let directory = tempfile::Builder::new()
        .prefix("hv-win-audio-")
        .tempdir_in("/tmp")
        .unwrap();
    let path = directory.path();
    eprintln!("Windows HDA S1 directory: {}", path.display());
    let disk = path.join("disk.qcow2");
    vmlab_disk::ops::create_linked_clone(&template, &disk).unwrap();
    std::fs::copy(vars, path.join("vars.fd")).unwrap();
    let description = json!({
        "machine":"q35", "name":"windows-audio-s1", "arch":"x86_64", "accel":"kvm",
        "vcpus":4, "memory_mib":8192,
        "hyperv":["relaxed","vapic","spinlocks","vpindex","runtime","time","synic","stimer","frequencies","reset","tlbflush","ipi"],
        "boot":{"kind":"firmware","firmware":"ovmf", "code":code,
            "vars":path.join("vars.fd"),"secure_boot":true},
        "disks":[{"id":"disk0","pci":{"bus":0,"device":2,"function":0},
            "path":disk,"format":"qcow2","read_only":false,"vmstate":true}],
        "console_pci":{"bus":0,"device":1,"function":0},
        "display":{"device":"virtio-vga","pci":{"bus":0,"device":3,"function":0}},
        "ports":[{"name":"vmlab.agent.0","socket":path.join("agent.sock")}],
        "nics":[],"serial_log":path.join("serial.log"),
        "audio":{"device":"hda","microphone":false,
            "node_name":"vmlab.test.windows-audio","node_description":"Windows HDA S1",
            "pci":{"bus":0,"device":27,"function":0}}
    });
    std::fs::write(
        path.join("machine.json"),
        serde_json::to_vec(&description).unwrap(),
    )
    .unwrap();
    let pipewire = std::env::var_os("VMLAB_HV_TEST_AUDIO_PIPEWIRE").map(PathBuf::from);
    if let Some(remote) = &pipewire {
        assert!(
            remote.is_absolute(),
            "PipeWire socket path must be absolute"
        );
        std::os::unix::fs::symlink(remote, path.join("pipewire-0")).unwrap();
    }
    let binary = if pipewire.is_some() {
        vmlab_hv_tests::hv_binary()
    } else {
        vmlab_hv_tests::hv_binary_with(&["testing"])
    };
    let log = std::fs::File::create(path.join("hv.log")).unwrap();
    let mut command = Command::new(binary);
    command
        .arg("--machine")
        .arg(path.join("machine.json"))
        .arg("--control")
        .arg(path.join("control.sock"))
        .arg("--paused")
        .env("XDG_RUNTIME_DIR", path)
        .env("XDG_STATE_HOME", path)
        .env("VMLAB_SKIP_UPDATES", "1")
        .stdout(Stdio::from(log.try_clone().unwrap()))
        .stderr(Stdio::from(log));
    if pipewire.is_none() {
        command.env("VMLAB_HV_TEST_AUDIO_PLAYBACK", path.join("playback.pcm"));
    }
    let mut vm = Vm {
        child: ManagedChild::spawn(&mut command).unwrap(),
        dir: directory,
    };
    let path = vm.dir.path();
    let deadline = Instant::now() + Duration::from_secs(30);
    let mut control = loop {
        if let Ok(client) = Client::connect(path.join("control.sock")) {
            break client;
        }
        assert!(vm.child.try_wait().unwrap().is_none(), "hypervisor exited");
        assert!(Instant::now() < deadline, "control socket timed out");
        std::thread::sleep(Duration::from_millis(25));
    };
    assert!(matches!(
        control.call(Control::Resume).unwrap(),
        Outcome::Ok { .. }
    ));
    let mut agent =
        AgentClient::connect(&path.join("agent.sock"), Duration::from_secs(300)).unwrap();
    powershell(
        &mut agent,
        r#"
$ErrorActionPreference = 'Stop'
Start-Service AudioEndpointBuilder
Start-Service Audiosrv
for ($n = 0; $n -lt 60; $n++) {
    $codec = @(Get-PnpDevice -PresentOnly | Where-Object { $_.InstanceId -like 'HDAUDIO\*' })
    $endpoints = @(Get-PnpDevice -PresentOnly -Class AudioEndpoint -ErrorAction SilentlyContinue | Where-Object { $_.Status -eq 'OK' -and $_.InstanceId -like 'SWD\MMDEVAPI\{0.0.0.*' })
    if ($codec.Count -gt 0 -and $codec[0].Status -eq 'OK' -and $endpoints.Count -gt 0) { break }
    Start-Sleep -Milliseconds 500
}
@{codec = @($codec | Select-Object Status,InstanceId,Problem); endpoints = @($endpoints | Select-Object Status,FriendlyName)} | ConvertTo-Json -Depth 4 -Compress
if ($codec.Count -eq 0 -or $codec[0].Status -ne 'OK' -or $endpoints.Count -eq 0) { exit 1 }
"#,
    );

    let tone = include_str!("audio_windows.ps1");
    if let Some(remote) = pipewire {
        let node = std::thread::scope(|scope| {
            let playback = scope.spawn(|| powershell(&mut agent, tone));
            let deadline = Instant::now() + Duration::from_secs(30);
            let node = loop {
                if let Some(node) = pipewire_node(&remote) {
                    break Some(node);
                }
                if playback.is_finished() || Instant::now() >= deadline {
                    break None;
                }
                std::thread::sleep(Duration::from_millis(100));
            };
            playback.join().unwrap();
            node
        });
        assert!(node.is_some(), "no running Windows PipeWire playback node");
        let node = node.unwrap();
        eprintln!(
            "Windows PipeWire stream: id={} name={} state={} format={}",
            node["id"],
            node["info"]["props"]["node.name"],
            node["info"]["state"],
            node["info"]["params"]["Format"]
        );
    } else {
        powershell(&mut agent, tone);
        let pcm = std::fs::read(path.join("playback.pcm")).unwrap();
        assert!(
            pcm.len() >= 48000 * 4,
            "less than one second of Windows PCM"
        );
        let samples: Vec<_> = pcm
            .as_chunks::<4>()
            .0
            .iter()
            .map(|frame| i16::from_le_bytes([frame[0], frame[1]]))
            .collect();
        let peak = samples
            .iter()
            .map(|sample| sample.unsigned_abs())
            .max()
            .unwrap();
        assert!(peak > 0, "Windows PCM is silent");
        assert!(
            samples.windows(48000).step_by(12000).any(|window| {
                let crossings = window
                    .windows(2)
                    .filter(|pair| pair[0] <= 0 && pair[1] > 0)
                    .count();
                (430..=450).contains(&crossings)
            }),
            "Windows PCM does not contain the 440 Hz tone; peak={peak}"
        );
        eprintln!(
            "Windows playback: {} PCM bytes, peak={peak}, 440 Hz verified",
            pcm.len()
        );
    }
    assert!(matches!(
        control.call(Control::Quit).unwrap(),
        Outcome::Ok { .. }
    ));
    assert!(vm.child.wait().unwrap().success());
}
