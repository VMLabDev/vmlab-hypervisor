//! Windows tablet and viewer input acceptance through S1 on a disposable overlay.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, AtomicU64, Ordering},
};
use std::time::{Duration, Instant};
use vmlab_hv_display::Message;
use vmlab_hv_proto::{
    client::Client,
    control::{Command as Control, Outcome},
};
use vmlab_hv_tests::{agent::AgentClient, process::ManagedChild};
use vmlab_hv_viewer::{Core, input::pointer};

struct Diagnostics<'a>(&'a Path);

struct DisplayReader {
    client: Arc<vmlab_hv_display::Client>,
    error: Arc<Mutex<Option<String>>>,
    frames: Arc<AtomicU64>,
    nonblack: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}
impl DisplayReader {
    fn new(client: Arc<vmlab_hv_display::Client>) -> Self {
        let source = client.clone();
        let error = Arc::new(Mutex::new(None));
        let failed = error.clone();
        let frames = Arc::new(AtomicU64::new(0));
        let count = frames.clone();
        let nonblack = Arc::new(AtomicBool::new(false));
        let painted = nonblack.clone();
        let thread = std::thread::spawn(move || {
            let mut core = Core::new(source.clone(), "ctrl+alt").unwrap();
            loop {
                let result = source.receive().and_then(|packet| {
                    if let Some(frame) = core.receive(packet)? {
                        let active = !frame.flags.has(vmlab_hv_display::FrameFlags::PLACEHOLDER)
                            && core.surfaces[&frame.surface]
                                .pixels
                                .as_chunks::<4>()
                                .0
                                .iter()
                                .any(|pixel| pixel[..3] != [0, 0, 0]);
                        painted.store(active, Ordering::SeqCst);
                        core.release(&frame)?;
                        count.fetch_add(1, Ordering::SeqCst);
                    }
                    Ok(())
                });
                if let Err(error) = result {
                    *failed.lock().unwrap() = Some(error.to_string());
                    break;
                }
            }
        });
        Self {
            client,
            error,
            frames,
            nonblack,
            thread: Some(thread),
        }
    }
}
impl Drop for DisplayReader {
    fn drop(&mut self) {
        let _ = self.client.stream().shutdown(std::net::Shutdown::Both);
        self.thread.take().unwrap().join().unwrap();
    }
}
impl Drop for Diagnostics<'_> {
    fn drop(&mut self) {
        if std::thread::panicking() {
            let log = std::fs::read_to_string(self.0).unwrap_or_default();
            eprintln!("hypervisor log:\n{log}");
        }
    }
}

fn template() -> Option<PathBuf> {
    if let Some(path) = std::env::var_os("VMLAB_HV_VIEWER_WINDOWS") {
        return Some(path.into());
    }
    let root = PathBuf::from(std::env::var_os("HOME")?)
        .join(".local/share/vmlab/templates/x86_64/windows-11");
    let mut disks: Vec<_> = std::fs::read_dir(root)
        .ok()?
        .filter_map(Result::ok)
        .map(|e| e.path().join("variants/standard/disk.qcow2"))
        .filter(|p| p.is_file())
        .collect();
    disks.sort();
    disks.pop()
}

fn powershell(agent: &mut AgentClient, script: &str) -> Vec<u8> {
    let output = agent
        .exec(
            &["powershell.exe", "-NoProfile", "-Command", script],
            Duration::from_secs(60),
        )
        .unwrap();
    assert_eq!(
        output.exit_code,
        0,
        "Windows: {}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    output.stdout
}

fn tablet_capabilities(agent: &mut AgentClient) {
    let probe = include_str!("windows_hid.ps1");
    let script =
        format!("{probe}\nConvertTo-Json -InputObject ([ViewerHid]::Capabilities()) -Compress");
    let caps: Vec<u16> = serde_json::from_slice(&powershell(agent, &script)).unwrap();
    assert_eq!(
        caps,
        [2, 1, 7],
        "Windows must parse a mouse with six input bytes plus report ID"
    );
    eprintln!("Windows HID usage/page/report length: {caps:?}");
}

fn spawn(binary: &Path, machine: &Path, control: &Path, stderr: &Path) -> ManagedChild {
    ManagedChild::spawn(
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
            .arg(control)
            .arg("--paused")
            .stdout(Stdio::null())
            .stderr(
                std::fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(stderr)
                    .unwrap(),
            ),
    )
    .unwrap()
}

#[test]
fn windows_tablet_and_first_release_chord_via_s1() {
    if !Path::new("/dev/kvm").exists() {
        eprintln!("SKIP Windows viewer S1: /dev/kvm unavailable");
        return;
    }
    let Some(template) = template() else {
        eprintln!(
            "SKIP Windows viewer S1: Windows 11 standard template absent; set VMLAB_HV_VIEWER_WINDOWS"
        );
        return;
    };
    let firmware = std::env::var_os("VMLAB_HV_FIRMWARE_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            PathBuf::from(std::env::var_os("HOME").unwrap())
                .join(".local/share/vmlab/guest/firmware/x86_64")
        });
    for path in [
        firmware.join("OVMF_CODE_4M.secboot.fd"),
        firmware.join("OVMF_VARS_4M.ms.fd"),
        PathBuf::from("/usr/share/qemu/vgabios-virtio.bin"),
    ] {
        if !path.is_file() {
            eprintln!("SKIP Windows viewer S1: {} unavailable", path.display());
            return;
        }
    }
    let directory = tempfile::Builder::new()
        .prefix("hv-viewer-windows-")
        .tempdir_in("/tmp")
        .unwrap();
    let disk = directory.path().join("disk.qcow2");
    vmlab_disk::ops::create_linked_clone(&template, &disk).unwrap();
    let vars = directory.path().join("vars.fd");
    std::fs::copy(firmware.join("OVMF_VARS_4M.ms.fd"), &vars).unwrap();
    let control = directory.path().join("control.sock");
    let display = directory.path().join("display.sock");
    let agent = directory.path().join("agent.sock");
    let machine = directory.path().join("machine.json");
    let stderr = directory.path().join("stderr.log");
    std::fs::write(&machine, serde_json::to_vec(&serde_json::json!({
        "machine":"q35", "name":"viewer-windows-s1", "arch":"x86_64", "accel":"kvm", "vcpus":4, "memory_mib":8192,
        "hyperv":["relaxed","vapic","spinlocks","vpindex","runtime","time","synic","stimer","frequencies","reset","tlbflush","ipi"],
        "boot":{"kind":"firmware","firmware":"ovmf", "code":firmware.join("OVMF_CODE_4M.secboot.fd"),"vars":vars,"secure_boot":true},
        "disks":[{"id":"disk0","pci":{"bus":0,"device":2,"function":0},"path":disk,"format":"qcow2","read_only":false,"vmstate":true}],
        "console_pci":{"bus":0,"device":1,"function":0},
        "display":{"device":"virtio-vga","pci":{"bus":0,"device":3,"function":0},"socket":display},
        "tablet":{"bus":0,"device":4,"function":0},
        "ports":[{"name":"vmlab.agent.0","socket":agent}],"nics":[],"serial_log":directory.path().join("serial.log")
    })).unwrap()).unwrap();
    let binary = std::env::var_os("VMLAB_HV_BINARY")
        .map(PathBuf::from)
        .unwrap_or_else(vmlab_hv_tests::hv_binary);
    let mut process = spawn(&binary, &machine, &control, &stderr);
    let diagnostics = || std::fs::read_to_string(&stderr).unwrap_or_default();
    let _diagnostics = Diagnostics(&stderr);
    let deadline = Instant::now() + Duration::from_secs(20);
    let mut control = loop {
        if let Ok(client) = Client::connect(&control) {
            break client;
        }
        assert!(process.try_wait().unwrap().is_none(), "{}", diagnostics());
        assert!(
            Instant::now() < deadline,
            "control timeout: {}",
            diagnostics()
        );
        std::thread::sleep(Duration::from_millis(20));
    };
    assert!(matches!(
        control.call(Control::Resume).unwrap(),
        Outcome::Ok { .. }
    ));
    let mut agent = AgentClient::connect(&agent, Duration::from_secs(240))
        .unwrap_or_else(|e| panic!("{e}: {}", diagnostics()));
    let devices = powershell(
        &mut agent,
        r#"for ($n=0; $n -lt 60; $n++) { $d=Get-PnpDevice -PresentOnly; if ($d | Where-Object { $_.Class -eq 'Mouse' -and $_.FriendlyName -eq 'HID-compliant mouse' -and $_.Status -eq 'OK' }) { $d | Where-Object { $_.Class -in @('USB','Mouse') } | Format-List Class,FriendlyName,Status; exit 0 }; Start-Sleep -Milliseconds 500 }; $d | Where-Object { $_.Class -in @('USB','Mouse') } | Format-List Class,FriendlyName,Status; exit 1"#,
    );
    eprintln!("Windows tablet: {}", String::from_utf8_lossy(&devices));
    tablet_capabilities(&mut agent);
    let (width, height) = (1024, 768);
    let mut core = Core::connect(&display, true, "ctrl+alt").unwrap();
    let reader = DisplayReader::new(core.client.clone());
    core.input.grab();
    for position in [
        (0., 0.),
        (321., 241.),
        ((width - 1) as f64, (height - 1) as f64),
    ] {
        core.send(pointer(position, (width, height), (width, height)))
            .unwrap();
        std::thread::sleep(Duration::from_millis(250));
    }
    for bit in [1, 2, 4] {
        for down in [true, false] {
            let messages = core.input.button(bit, down);
            core.send(messages).unwrap();
            std::thread::sleep(Duration::from_millis(250));
        }
    }
    for (key, down) in [(29, true), (56, true)] {
        let messages = core.input.key(key, down);
        core.send(messages).unwrap();
        std::thread::sleep(Duration::from_millis(250));
    }
    assert!(!core.input.grabbed, "first Ctrl+Alt press must release");
    let before = reader.frames.load(Ordering::SeqCst);
    let started = Instant::now();
    for index in 0..20_000 {
        core.send([Message::PointerAbs {
            x: index % width,
            y: index % height,
            width,
            height,
        }])
        .unwrap();
        if index % 500 == 499 {
            core.send([
                Message::Buttons { mask: 1 },
                Message::Buttons { mask: 0 },
                Message::Scroll { dx: 0, dy: 1 },
            ])
            .unwrap();
        }
    }
    core.send([Message::PointerAbs {
        x: width - 1,
        y: height - 1,
        width,
        height,
    }])
    .unwrap();
    std::thread::sleep(Duration::from_secs(2));
    let error = reader.error.lock().unwrap().clone();
    assert!(
        error.is_none(),
        "Windows display disconnected during burst: {error:?}"
    );
    let after = reader.frames.load(Ordering::SeqCst);
    assert!(
        after > before,
        "Windows display stopped updating during input: {before} -> {after}"
    );
    assert!(
        reader.nonblack.load(Ordering::SeqCst),
        "Windows final frame is black or a host placeholder"
    );
    let log = diagnostics();
    assert!(
        !log.contains("USB tablet input queue full"),
        "Windows tablet backlog after 20,000 moves"
    );
    eprintln!(
        "Windows frame count during burst: {before} -> {after}; final active frame is not black"
    );
    eprintln!(
        "Windows 2D S1: 20,000 moves, 80 button transitions and 40 wheel events; no disconnect or tablet overflow in {:?}",
        started.elapsed()
    );
    drop(reader);
    drop(core);
    assert!(matches!(
        control.call(Control::Quit).unwrap(),
        Outcome::Ok { .. }
    ));
    assert!(process.wait().unwrap().success(), "{}", diagnostics());
}
