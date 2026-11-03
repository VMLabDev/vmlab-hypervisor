//! P1 macOS acceptance through control, native display, and the guest agent.

use std::os::unix::fs::FileExt;
use std::{
    fs::File,
    io::{Seek, Write},
    os::fd::FromRawFd,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    time::{Duration, Instant},
};
use vmlab_hv_proto::{
    client::Client,
    control::{Command as Control, Outcome},
};
use vmlab_hv_tests::{agent::AgentClient, process::ManagedChild};

fn exec(guest: &mut AgentClient, script: &str) -> String {
    let result = guest
        .exec(&["/bin/sh", "-c", script], Duration::from_secs(60))
        .unwrap();
    assert_eq!(
        result.exit_code,
        0,
        "guest command failed: {}",
        String::from_utf8_lossy(&result.stderr)
    );
    String::from_utf8(result.stdout).unwrap().trim().into()
}

fn control(client: &mut Client, command: Control) {
    let result = client.call(command).unwrap();
    assert!(matches!(result, Outcome::Ok { .. }), "{result:?}");
}

fn check_powerdown(status: std::process::ExitStatus, diagnostics: &str) -> Result<(), String> {
    if status.success() {
        Ok(())
    } else {
        Err(format!(
            "macOS agent powerdown exited with {status}: {diagnostics}"
        ))
    }
}

#[test]
fn powerdown_failure_reports_exit_status_and_stderr() {
    let output = Command::new("/bin/sh")
        .args(["-c", "printf shutdown-diagnostic >&2; exit 17"])
        .output()
        .unwrap();
    let message =
        check_powerdown(output.status, std::str::from_utf8(&output.stderr).unwrap()).unwrap_err();
    assert!(message.contains("17"), "{message}");
    assert!(message.contains("shutdown-diagnostic"), "{message}");
}

fn native_pixels(path: &Path) -> Vec<u8> {
    use vmlab_hv_display::{Capabilities, Message};
    let client =
        vmlab_hv_display::Client::connect(path, Capabilities(Capabilities::FRAMES)).unwrap();
    client
        .stream()
        .set_read_timeout(Some(Duration::from_secs(20)))
        .unwrap();
    let configure = client.receive().unwrap();
    assert!(
        matches!(
            configure.message,
            Message::SurfaceConfigure {
                width: 1280,
                height: 800,
                stride: 5120,
                ..
            }
        ),
        "native frame is not 1280x800 XRGB8888: {:?}",
        configure.message
    );
    let Message::FrameReady {
        surface_id,
        buffer,
        seq,
        ..
    } = client.receive().unwrap().message
    else {
        panic!("native frame not delivered")
    };
    let pixels = File::from(configure.fds[buffer as usize].try_clone().unwrap());
    let mut bytes = vec![0; 1280 * 800 * 4];
    pixels.read_exact_at(&mut bytes, 0).unwrap();
    client.release(surface_id, buffer, seq).unwrap();
    bytes
}

fn password_bullets(bytes: &[u8]) -> usize {
    let mut white = vec![false; 110 * 16];
    for y in 0..16 {
        for x in 0..110 {
            let pixel = &bytes[((710 + y) * 1280 + 570 + x) * 4..][..3];
            white[y * 110 + x] = pixel.iter().all(|channel| *channel > 170);
        }
    }
    let mut count = 0;
    for seed in 0..white.len() {
        if !std::mem::take(&mut white[seed]) {
            continue;
        }
        let mut pending = vec![seed];
        let mut area = 0;
        let (mut min_x, mut max_x, mut min_y, mut max_y) = (110, 0, 16, 0);
        while let Some(at) = pending.pop() {
            let (x, y) = (at % 110, at / 110);
            area += 1;
            min_x = min_x.min(x);
            max_x = max_x.max(x);
            min_y = min_y.min(y);
            max_y = max_y.max(y);
            for (nx, ny) in [
                (x.wrapping_sub(1), y),
                (x + 1, y),
                (x, y.wrapping_sub(1)),
                (x, y + 1),
            ] {
                if nx < 110 && ny < 16 && std::mem::take(&mut white[ny * 110 + nx]) {
                    pending.push(ny * 110 + nx);
                }
            }
        }
        if (3..=7).contains(&(max_x - min_x + 1))
            && (3..=7).contains(&(max_y - min_y + 1))
            && (18..=40).contains(&area)
        {
            count += 1;
        }
    }
    count
}

fn key(client: &vmlab_hv_display::Client, qcode: &str, down: bool) {
    client
        .send(&vmlab_hv_display::Message::Key {
            qcode: qcode.into(),
            down,
        })
        .unwrap();
}

fn clear_password(input: &vmlab_hv_display::Client) {
    key(input, "meta_l", true);
    key(input, "a", true);
    key(input, "a", false);
    key(input, "meta_l", false);
    key(input, "backspace", true);
    key(input, "backspace", false);
}

fn keyboard_input(native: &Path, input: &vmlab_hv_display::Client, guest: &mut AgentClient) {
    use vmlab_hv_display::Message;
    let deadline = Instant::now() + Duration::from_secs(300);
    loop {
        let bytes = native_pixels(native);
        let brightness = |x: usize, y: usize| -> u16 {
            bytes[(y * 1280 + x) * 4..][..3]
                .iter()
                .map(|v| u16::from(*v))
                .sum::<u16>()
                / 3
        };
        let field = brightness(680, 718);
        if (70..170).contains(&field)
            && brightness(550, 718) > field + 20
            && brightness(680, 695) > field + 20
        {
            break;
        }
        if Instant::now() >= deadline {
            eprintln!(
                "macOS UI processes: {}",
                exec(
                    guest,
                    "ps -axo pid,comm | grep -E '(WindowServer|loginwindow|SecurityAgent)' || true"
                )
            );
            super::display_acceptance::diagnostic_screenshot(
                native,
                &native.parent().unwrap().join("keyboard-failure.ppm"),
            );
            panic!("macOS login password field did not appear");
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    let deadline = Instant::now() + Duration::from_secs(20);
    'focus: loop {
        input
            .send(&Message::PointerAbs {
                x: 640,
                y: 718,
                width: 1280,
                height: 800,
            })
            .unwrap();
        input.send(&Message::Buttons { mask: 1 }).unwrap();
        std::thread::sleep(Duration::from_millis(50));
        input.send(&Message::Buttons { mask: 0 }).unwrap();
        clear_password(input);
        key(input, "h", true);
        key(input, "h", false);
        let attempt = (Instant::now() + Duration::from_secs(2)).min(deadline);
        loop {
            if password_bullets(&native_pixels(native)) == 1 {
                break 'focus;
            }
            if Instant::now() >= deadline {
                super::display_acceptance::diagnostic_screenshot(
                    native,
                    &native.parent().unwrap().join("keyboard-failure.ppm"),
                );
                panic!("macOS password field did not accept the keyboard focus probe");
            }
            if Instant::now() >= attempt {
                break;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    }
    eprintln!("macOS P1 password focus confirmed by one native-frame glyph");
    for amount in [5, 3] {
        clear_password(input);
        for qcode in ["h", "w", "r", "e", "f"].into_iter().take(amount) {
            key(input, qcode, true);
            key(input, qcode, false);
        }
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            if password_bullets(&native_pixels(native)) == amount {
                break;
            }
            if Instant::now() >= deadline {
                super::display_acceptance::diagnostic_screenshot(
                    native,
                    &native.parent().unwrap().join("keyboard-failure.ppm"),
                );
                panic!("USB keyboard did not produce {amount} password glyphs");
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    }
}

#[test]
fn password_glyph_detector_matches_the_captured_five_key_exercise() {
    let image = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../docs/hwref/q35-macos-tahoe/hid-input.png");
    let output = match Command::new("magick")
        .arg(image)
        .args(["-depth", "8", "BGRA:-"])
        .output()
    {
        Ok(output) => output,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            eprintln!("skipping password mask fixture decoder: ImageMagick unavailable");
            return;
        }
        Err(error) => panic!("ImageMagick failed: {error}"),
    };
    assert!(output.status.success());
    assert_eq!(output.stdout.len(), 1280 * 800 * 4);
    assert_eq!(password_bullets(&output.stdout), 5);
}

#[test]
#[cfg_attr(vmlab_ci_no_kvm, ignore = "requires KVM and/or local guest assets")]
fn macos_tahoe_p1_agent_display_keyboard_network_reset_snapshot_stop() {
    // The live macOS boot is intermittent (OpenCore and early-boot stalls) and is
    // parked under https://github.com/VMLabDev/vmlab/issues/200, so it runs only on
    // request until that issue is picked up.
    if std::env::var_os("VMLAB_HV_MACOS_ACCEPTANCE").is_none() {
        eprintln!(
            "skipping macOS P1: opt-in while parked under vmlab#200; set VMLAB_HV_MACOS_ACCEPTANCE=1"
        );
        return;
    }
    if !Path::new("/dev/kvm").exists() {
        eprintln!("skipping macOS P1: /dev/kvm unavailable");
        return;
    }
    let Some(template) = super::acceptance::template("macos-tahoe", "VMLAB_HV_MACOS_DISK") else {
        eprintln!("skipping macOS P1: macos-tahoe template absent; set VMLAB_HV_MACOS_DISK");
        return;
    };
    let Some(key) = vmlab_hv_tests::host_key::load_host_osk()
        .expect("loading host key failed; contents suppressed")
    else {
        eprintln!(
            "skipping macOS P1: host.apple_smc_osk absent; alternatively set VMLAB_HV_TEST_APPLE_SMC_OSK"
        );
        return;
    };
    let firmware = std::env::var_os("VMLAB_HV_FIRMWARE_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            PathBuf::from(std::env::var_os("HOME").unwrap())
                .join(".local/share/vmlab/guest/firmware/x86_64")
        });
    for file in ["OVMF_CODE_4M.fd", "OVMF_VARS_4M.fd"] {
        assert!(firmware.join(file).is_file(), "missing firmware {file}");
    }
    let mut directory = tempfile::Builder::new()
        .prefix("hv-macos-")
        .tempdir()
        .unwrap();
    if std::env::var_os("VMLAB_HV_ACCEPTANCE_KEEP").is_some() {
        directory.disable_cleanup(true);
        eprintln!("macOS P1 artifacts: {}", directory.path().display());
    }
    let path = directory.path().to_path_buf();
    let disk = path.join("disk.qcow2");
    vmlab_disk::ops::create_linked_clone(&template, &disk).unwrap();
    let vars = path.join("vars.fd");
    std::fs::copy(firmware.join("OVMF_VARS_4M.fd"), &vars).unwrap();
    let socket = path.join("control.sock");
    let agent = path.join("agent.sock");
    let network = path.join("net.sock");
    let _switch = vmlab_hv_tests::switch::MiniSwitch::bind(&network).unwrap();
    let serial = path.join("serial.log");
    let stderr = path.join("stderr.log");
    let mut description = serde_json::json!({
        "name":"macos-p1", "machine":"q35", "arch":"x86_64", "accel":"kvm", "vcpus":4,"memory_mib":8192,
        "cpu_model":"Skylake-Client-v4", "invtsc":true,"vmware_cpuid_freq":true,"msr_policy":"ignore",
        "usb_keyboard":true,"smbios_type2":true,
        "boot":{"kind":"firmware","firmware":"ovmf","code":firmware.join("OVMF_CODE_4M.fd"),"vars":vars},
        "ahci_pci":{"bus":0,"device":31,"function":2},
        "console_pci":{"bus":0,"device":1,"function":0},
        "display":{"device":"vmware-svga","pci":{"bus":0,"device":3,"function":0},"rom":null},
        "tablet":{"bus":0,"device":4,"function":0},
        "ports":[{"name":"vmlab.agent.0","socket":agent}],
        "disks":[{"id":"disk0","bus":"ahci","port":0,"path":disk,"format":"qcow2","read_only":false,"vmstate":true}],
        "nics":[{"mac":"52:54:00:12:34:56","mtu":1500,"attachment":{"kind":"stream","socket":network},"pci":{"bus":0,"device":2,"function":0}}],
        "serial_log":serial
    });
    // Keep the launch secret in an inherited anonymous memfd, never an artifact.
    // SAFETY: memfd_create receives a valid NUL-terminated name and no flags.
    let fd = unsafe { libc::memfd_create(c"macos-launch".as_ptr(), 0) };
    assert!(fd >= 0);
    // SAFETY: fd is a fresh, exclusively owned descriptor.
    let mut machine = unsafe { File::from_raw_fd(fd) };
    std::fs::write(
        path.join("machine-redacted.json"),
        serde_json::to_vec_pretty(&description).unwrap(),
    )
    .unwrap();
    description["applesmc"] = serde_json::json!({"osk":key});
    machine
        .write_all(&serde_json::to_vec(&description).unwrap())
        .unwrap();
    machine.rewind().unwrap();
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
            .arg(&binary)
            .arg("--machine")
            .arg(format!("/proc/{}/fd/{fd}", std::process::id()))
            .arg("--control")
            .arg(&socket)
            .arg("--paused")
            .env("VMLAB_HV_DIAGNOSTICS", "1")
            .stdout(Stdio::null())
            .stderr(File::create(&stderr).unwrap()),
    )
    .unwrap();
    let diagnostics = || std::fs::read_to_string(&stderr).unwrap_or_default();
    let deadline = Instant::now() + Duration::from_secs(30);
    let mut client = loop {
        if let Ok(client) = Client::connect(&socket) {
            break client;
        }
        assert!(
            process.try_wait().unwrap().is_none(),
            "macOS launch failed: {}",
            diagnostics()
        );
        assert!(
            Instant::now() < deadline,
            "control timeout: {}",
            diagnostics()
        );
        std::thread::sleep(Duration::from_millis(20));
    };
    let host_ignore_before =
        std::fs::read_to_string("/sys/module/kvm/parameters/ignore_msrs").unwrap();
    if std::env::var_os("VMLAB_HV_TEST_REQUIRE_MSR_EXITS").is_some() {
        assert_eq!(
            host_ignore_before.trim(),
            "N",
            "MSR proof requires ignore_msrs=N"
        );
    }
    control(&mut client, Control::Resume);
    let timeout = Duration::from_secs(
        std::env::var("VMLAB_HV_TEMPLATE_TIMEOUT")
            .ok()
            .map(|s| s.parse().unwrap())
            .unwrap_or(300),
    );
    let mut guest = match AgentClient::connect(&agent, timeout) {
        Ok(guest) => guest,
        Err(error) => {
            super::display_acceptance::diagnostic_screenshot(
                &path.join("display.sock"),
                &path.join("failure.ppm"),
            );
            control(&mut client, Control::Quit);
            process.wait().unwrap();
            panic!("macOS agent boot failed: {error}; {}", diagnostics());
        }
    };
    assert_eq!(
        exec(&mut guest, "printf macos-agent-ready"),
        "macos-agent-ready"
    );
    let tsc = exec(&mut guest, "sysctl -n machdep.tsc.frequency");
    eprintln!("macOS P1 agent-ready; TSC Hz={tsc}");
    let native = path.join("display.sock");
    let bytes = native_pixels(&native);
    assert!(
        bytes
            .as_chunks::<4>()
            .0
            .iter()
            .any(|pixel| pixel[..3] != [0, 0, 0]),
        "native frame contains no visible pixels"
    );
    let input = vmlab_hv_display::Client::connect(
        &native,
        vmlab_hv_display::Capabilities(vmlab_hv_display::Capabilities::INPUT),
    )
    .unwrap();
    eprintln!(
        "macOS USB inventory: {}",
        exec(&mut guest, "ioreg -p IOUSB | head -40")
    );
    keyboard_input(&native, &input, &mut guest);
    eprintln!(
        "macOS P1 USB keyboard produced five and then three password glyphs before network and snapshot checks"
    );
    exec(
        &mut guest,
        "ifconfig en0 inet 10.42.0.50 netmask 255.255.255.0 up; ping -c 2 -t 10 10.42.0.1",
    );
    eprintln!("macOS P1 network ping passed");
    exec(
        &mut guest,
        "printf before > /Users/Shared/vmlab-snapshot-check; sync",
    );
    directory.disable_cleanup(false);
    control(
        &mut client,
        Control::SnapshotSave {
            name: "macos".into(),
        },
    );
    exec(
        &mut guest,
        "printf after > /Users/Shared/vmlab-snapshot-check; sync",
    );
    control(
        &mut client,
        Control::SnapshotLoad {
            name: "macos".into(),
        },
    );
    guest.resync(timeout).unwrap();
    eprintln!("macOS P1 online snapshot restored and agent re-handshook");
    assert_eq!(
        exec(&mut guest, "cat /Users/Shared/vmlab-snapshot-check"),
        "before"
    );
    assert_eq!(exec(&mut guest, "sysctl -n machdep.tsc.frequency"), tsc);
    let boot_uuid = exec(&mut guest, "sysctl -n kern.bootsessionuuid");
    client.take_events();
    guest.shutdown("reboot", Duration::from_secs(10)).unwrap();
    drop(guest);
    let deadline = Instant::now() + timeout;
    loop {
        let event = client
            .wait_event_timeout(deadline.saturating_duration_since(Instant::now()))
            .unwrap();
        if matches!(event, Some(vmlab_hv_proto::control::Event::Reset)) {
            break;
        }
        assert!(
            event.is_some() && Instant::now() < deadline,
            "macOS reboot did not reach the S1 reset event"
        );
    }
    let mut guest = AgentClient::connect(&agent, timeout).unwrap();
    assert_ne!(
        exec(&mut guest, "sysctl -n kern.bootsessionuuid"),
        boot_uuid
    );
    eprintln!("macOS P1 reset event and new boot session verified");
    assert_eq!(exec(&mut guest, "sysctl -n machdep.tsc.frequency"), tsc);
    keyboard_input(&native, &input, &mut guest);
    eprintln!(
        "macOS P1 USB keyboard produced five and then three password glyphs in native frames"
    );
    guest
        .shutdown("powerdown", Duration::from_secs(10))
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(90);
    while process.try_wait().unwrap().is_none() {
        assert!(
            Instant::now() < deadline,
            "macOS did not stop through agent: {}",
            diagnostics()
        );
        std::thread::sleep(Duration::from_millis(100));
    }
    let status = process.wait().unwrap();
    check_powerdown(status, &diagnostics()).unwrap();
    let ignored: Vec<_> = diagnostics()
        .lines()
        .filter(|line| line.contains("ignored unknown MSR"))
        .map(str::to_owned)
        .collect();
    eprintln!("macOS P1 ignored MSRs: {ignored:?}");
    let host_ignore = std::fs::read_to_string("/sys/module/kvm/parameters/ignore_msrs").unwrap();
    if host_ignore_before.trim() == "N" && host_ignore.trim() == "N" {
        assert!(
            !ignored.is_empty(),
            "macOS boot produced no unknown-MSR exits"
        );
        assert_eq!(
            ignored
                .iter()
                .collect::<std::collections::BTreeSet<_>>()
                .len(),
            ignored.len(),
            "MSR indices were logged again after snapshot or reset"
        );
        eprintln!("macOS P1 passed with host ignore_msrs=N before boot and after shutdown");
    } else {
        assert!(
            std::env::var_os("VMLAB_HV_TEST_REQUIRE_MSR_EXITS").is_none(),
            "host MSR policy changed during the required verification run"
        );
        eprintln!(
            "macOS P1 independent checks passed; unknown-MSR independence remains unverified until the owner sets ignore_msrs=N"
        );
    }
}
