//! S1 firmware consumption, observed through the native display protocol.

use std::fs::File;
use std::io::Write;
use std::os::unix::fs::FileExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::json;
use vmlab_hv_display::{Capabilities, Client as DisplayClient, Message, SurfaceKind};
use vmlab_hv_proto::client::Client;
use vmlab_hv_proto::control::{Command as Ctrl, Outcome};
use vmlab_hv_tests::process::ManagedChild;

fn firmware_directory() -> Option<PathBuf> {
    let Some(path) = std::env::var_os("VMLAB_HV_TEST_FIRMWARE_DIR") else {
        eprintln!(
            "SKIP fw_cfg firmware S1: set VMLAB_HV_TEST_FIRMWARE_DIR to vmlab's GOP-capable firmware bundle"
        );
        return None;
    };
    if !Path::new("/dev/kvm").exists() {
        eprintln!("SKIP fw_cfg firmware S1: /dev/kvm missing");
        return None;
    }
    Some(path.into())
}

fn screenshot(label: &str, width: u32, height: u32, stride: u32, pixels: &[u8]) {
    let Some(directory) = std::env::var_os("VMLAB_HV_TEST_FW_EVIDENCE_DIR") else {
        return;
    };
    let directory = PathBuf::from(directory);
    std::fs::create_dir_all(&directory).unwrap();
    let mut output = File::create(directory.join(format!("{label}.ppm"))).unwrap();
    write!(output, "P6\n{width} {height}\n255\n").unwrap();
    for row in pixels.chunks_exact(stride as usize).take(height as usize) {
        for pixel in row.as_chunks::<4>().0.iter().take(width as usize) {
            output.write_all(&[pixel[2], pixel[1], pixel[0]]).unwrap();
        }
    }
}

fn boot(label: &str, firmware: &Path, ovmf: bool, files: serde_json::Value) {
    let dir = tempfile::Builder::new()
        .prefix("hv-fwcfg-")
        .tempdir()
        .unwrap();
    let path = dir.path();
    let code = firmware.join(if ovmf {
        "OVMF_CODE_4M.fd"
    } else {
        "bios-vmlab-q35.bin"
    });
    assert!(code.is_file(), "missing firmware {}", code.display());
    let mut boot =
        json!({"kind":"firmware", "firmware":if ovmf {"ovmf"} else {"seabios"}, "code":code});
    boot["boot_menu"] = json!(!ovmf);
    if ovmf {
        let vars = path.join("vars.fd");
        std::fs::copy(firmware.join("OVMF_VARS_4M.fd"), &vars).unwrap();
        boot["vars"] = json!(vars);
    }
    let disk = path.join("state.qcow2");
    vmlab_disk::ops::create_qcow2(&disk, 64 << 20, None, None).unwrap();
    let description = json!({
        "machine":"q35", "name":format!("fwcfg-{label}"), "arch":"x86_64", "accel":"kvm",
        "vcpus":1, "memory_mib":256, "boot":boot, "fw_cfg":files,
        "display":{"device":"std-vga", "pci":{"bus":0,"device":4,"function":0}},
        "disks":[{"id":"state", "path":disk, "format":"qcow2", "read_only":false, "vmstate":true,
                  "pci":{"bus":0,"device":5,"function":0}}],
        "nics":[], "ports":[], "serial_log":path.join("serial.log")
    });
    let machine = path.join("machine.json");
    std::fs::write(&machine, serde_json::to_vec(&description).unwrap()).unwrap();
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
            .arg(env!("CARGO_BIN_EXE_vmlab-hv"))
            .arg("--machine")
            .arg(machine)
            .arg("--control")
            .arg(path.join("control.sock"))
            .arg("--paused")
            .stdout(Stdio::null())
            .stderr(File::create(path.join("hv.log")).unwrap()),
    )
    .unwrap();
    let deadline = Instant::now() + Duration::from_secs(30);
    let mut control = loop {
        if let Ok(client) = Client::connect(path.join("control.sock")) {
            break client;
        }
        assert!(
            Instant::now() < deadline && child.try_wait().unwrap().is_none(),
            "firmware launch failed: {}",
            std::fs::read_to_string(path.join("hv.log")).unwrap()
        );
        std::thread::sleep(Duration::from_millis(10));
    };
    let mut display = DisplayClient::connect(
        path.join("display.sock"),
        Capabilities(Capabilities::FRAMES | Capabilities::CPU_COPY),
    )
    .unwrap();
    display
        .stream()
        .set_read_timeout(Some(Duration::from_secs(10)))
        .unwrap();
    assert!(matches!(
        control.call(Ctrl::Resume).unwrap(),
        Outcome::Ok { .. }
    ));
    let mut dimensions = (0, 0, 0);
    let mut buffers = Vec::new();
    let mut restored = false;
    loop {
        let packet = display.receive().unwrap_or_else(|error| {
            panic!("{label}: display read failed at {dimensions:?}: {error}; hypervisor: {}; serial: {}",
                std::fs::read_to_string(path.join("hv.log")).unwrap_or_default(),
                std::fs::read_to_string(path.join("serial.log")).unwrap_or_default())
        });
        match packet.message {
            Message::SurfaceConfigure {
                width,
                height,
                stride,
                kind: SurfaceKind::Shm,
                ..
            } => {
                dimensions = (width, height, stride);
                buffers = packet.fds;
            }
            Message::FrameReady {
                surface_id,
                buffer,
                seq,
                ..
            } => {
                let (width, height, stride) = dimensions;
                let mut pixels = vec![0; (stride * height) as usize];
                File::from(buffers[buffer as usize].try_clone().unwrap())
                    .read_exact_at(&mut pixels, 0)
                    .unwrap();
                display.release(surface_id, buffer, seq).unwrap();
                screenshot(&format!("{label}-last"), width, height, stride, &pixels);
                let matched = if ovmf {
                    (width, height) == (1024, 768) && pixels.iter().any(|byte| *byte != 0)
                } else if width >= 64 && height >= 64 {
                    let sample = |x: u32, y: u32| {
                        let offset = (y * stride + x * 4) as usize;
                        &pixels[offset..offset + 3]
                    };
                    let red = sample(width / 2 - 16, height / 2);
                    let green = sample(width / 2 + 16, height / 2);
                    red[2] > 220
                        && red[0] < 30
                        && red[1] < 30
                        && green[1] > 220
                        && green[0] < 30
                        && green[2] < 30
                } else {
                    false
                };
                if matched {
                    screenshot(
                        &format!("{label}-{}", if restored { "restored" } else { "boot" }),
                        width,
                        height,
                        stride,
                        &pixels,
                    );
                    if restored {
                        break;
                    }
                    assert!(matches!(
                        control.call(Ctrl::Pause).unwrap(),
                        Outcome::Ok { .. }
                    ));
                    for file in description["fw_cfg"].as_array().unwrap() {
                        if let Some(source) = file["source"]["path"].as_str() {
                            std::fs::remove_file(source).unwrap();
                        }
                    }
                    assert!(matches!(
                        control
                            .call(Ctrl::SnapshotSave {
                                name: "firmware".into()
                            })
                            .unwrap(),
                        Outcome::Ok { .. }
                    ));
                    assert!(matches!(
                        control
                            .call(Ctrl::SnapshotLoad {
                                name: "firmware".into()
                            })
                            .unwrap(),
                        Outcome::Ok { .. }
                    ));
                    // A new connection cannot consume a pre-restore queued frame.
                    display = DisplayClient::connect(
                        path.join("display.sock"),
                        Capabilities(Capabilities::FRAMES | Capabilities::CPU_COPY),
                    )
                    .unwrap();
                    display
                        .stream()
                        .set_read_timeout(Some(Duration::from_secs(10)))
                        .unwrap();
                    dimensions = (0, 0, 0);
                    buffers.clear();
                    restored = true;
                }
                assert!(
                    Instant::now() < deadline,
                    "{label}: firmware did not consume fw_cfg inputs; last size {width}x{height}"
                );
            }
            _ => {}
        }
    }
    assert!(matches!(
        control.call(Ctrl::Quit).unwrap(),
        Outcome::Ok { .. }
    ));
    assert!(child.wait().unwrap().success());
    eprintln!(
        "PASS {label}: firmware consumed fw_cfg inputs; native screenshot before and after S1 snapshot restore"
    );
}

#[test]
#[cfg_attr(vmlab_ci_no_kvm, ignore = "requires KVM and/or local guest assets")]
fn ovmf_honours_fw_cfg_gop_resolution() {
    let Some(firmware) = firmware_directory() else {
        return;
    };
    let features: serde_json::Value =
        serde_json::from_slice(&std::fs::read(firmware.join("features.json")).unwrap()).unwrap();
    assert_eq!(
        features["gop_resolution"], true,
        "firmware must advertise GOP hints"
    );
    boot(
        "ovmf",
        &firmware,
        true,
        json!([
            {"name":"opt/vmlab/gop-width", "source":{"kind":"inline", "bytes":b"1024"}},
            {"name":"opt/vmlab/gop-height", "source":{"kind":"inline", "bytes":b"768"}}
        ]),
    );
}

#[test]
#[cfg_attr(vmlab_ci_no_kvm, ignore = "requires KVM and/or local guest assets")]
fn seabios_displays_fw_cfg_jpeg_splash() {
    let Some(firmware) = firmware_directory() else {
        return;
    };
    let dir = tempfile::tempdir().unwrap();
    let jpeg = dir.path().join("splash.jpg");
    std::fs::write(&jpeg, include_bytes!("fixtures/fwcfg-splash.jpg")).unwrap();
    boot(
        "seabios",
        &firmware,
        false,
        json!([
            {"name":"bootsplash.jpg", "source":{"kind":"file", "path":jpeg}},
            {"name":"etc/boot-menu-wait", "source":{"kind":"inline", "bytes":10000u16.to_le_bytes()}}
        ]),
    );
}
