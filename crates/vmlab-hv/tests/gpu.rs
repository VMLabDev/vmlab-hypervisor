//! S1: real virgl rendering, native dma-buf frames and CpuCopy screenshots.

use serde_json::json;
use std::fs::File;
use std::os::unix::fs::FileExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};
use vmlab_hv_display::{Capabilities, Client as DisplayClient, Message, SurfaceKind};
use vmlab_hv_proto::{
    client::Client,
    control::{Command as Ctrl, Outcome},
};
use vmlab_hv_tests::{agent::AgentClient, process::ManagedChild};

fn render_node() -> Option<PathBuf> {
    if let Some(node) = std::env::var_os("VMLAB_HV_TEST_RENDER_NODE") {
        return Some(node.into());
    }
    let mut nodes: Vec<_> = std::fs::read_dir("/dev/dri")
        .ok()?
        .flatten()
        .filter(|e| e.file_name().to_string_lossy().starts_with("renderD"))
        .map(|e| e.path())
        .collect();
    nodes.sort_by_key(|node| {
        let vendor = std::fs::read_to_string(
            Path::new("/sys/class/drm")
                .join(node.file_name().unwrap())
                .join("device/vendor"),
        )
        .unwrap_or_default();
        (vendor.trim() != "0x1002", node.clone())
    });
    nodes.into_iter().find(|node| {
        std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(node)
            .is_ok()
    })
}

fn template() -> Option<PathBuf> {
    if let Some(path) = std::env::var_os("VMLAB_HV_TEST_VM_DISK") {
        return Some(path.into());
    }
    let root = PathBuf::from(std::env::var_os("HOME")?)
        .join(".local/share/vmlab/templates/x86_64/ubuntu-26.04");
    let mut paths: Vec<_> = std::fs::read_dir(root)
        .ok()?
        .flatten()
        .map(|e| e.path().join("disk.qcow2"))
        .filter(|p| p.is_file())
        .collect();
    paths.sort();
    paths.pop()
}

#[test]
fn ubuntu_virgl_scanout_and_cpu_copy() {
    let Some(node) = render_node() else {
        eprintln!("SKIP GPU S1: no usable /dev/dri render node");
        return;
    };
    let Some(template) = template() else {
        eprintln!("SKIP GPU S1: ubuntu-26.04 template missing");
        return;
    };
    if !Path::new("/dev/kvm").exists()
        || !Path::new("/usr/share/qemu/vgabios-virtio.bin").is_file()
        || !Path::new("/usr/share/qemu/bios-256k.bin").is_file()
    {
        eprintln!("SKIP GPU S1: KVM, SeaBIOS or SeaVGABIOS missing");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path();
    let overlay = path.join("disk.qcow2");
    let mode = std::env::var("VMLAB_HV_TEST_GPU_MODE").unwrap_or_else(|_| "virgl".into());
    vmlab_disk::ops::create_linked_clone(&template, &overlay).unwrap();
    let machine = json!({
        "name": "gpu-s1", "machine": "q35", "arch": "x86_64", "accel": "kvm", "vcpus": 2, "memory_mib": 4096,
        "boot": {"kind": "firmware", "firmware": "seabios", "code": "/usr/share/qemu/bios-256k.bin", "bootorder": ["/pci@i0cf8/scsi@2/disk@0,0"]},
        "console_pci": {"bus": 0, "device": 1, "function": 0},
        "display": {"device": "virtio-vga", "pci": {"bus": 0, "device": 4, "function": 0}, "rom": null, "gpu": mode, "render_node": node},
        "disks": [{"id": "root", "path": overlay, "format": "qcow2", "read_only": false, "vmstate": true, "pci": {"bus": 0, "device": 2, "function": 0}}],
        "nics": [], "serial_log": path.join("serial.log"), "ports": [{"name": "vmlab.agent.0", "socket": path.join("agent.sock")}]
    });
    std::fs::write(
        path.join("machine.json"),
        serde_json::to_vec(&machine).unwrap(),
    )
    .unwrap();
    let log = File::create(path.join("hv.log")).unwrap();
    let mut child = ManagedChild::spawn(
        Command::new(env!("CARGO_BIN_EXE_vmlab-hv"))
            .args([
                "--machine",
                path.join("machine.json").to_str().unwrap(),
                "--control",
                path.join("control.sock").to_str().unwrap(),
            ])
            .stdout(Stdio::from(log.try_clone().unwrap()))
            .stderr(Stdio::from(log)),
    )
    .unwrap();
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let deadline = Instant::now() + Duration::from_secs(120);
        let mut control = loop {
            if let Ok(control) = Client::connect(path.join("control.sock")) {
                break control;
            }
            assert!(
                Instant::now() < deadline && child.try_wait().unwrap().is_none(),
                "GPU VM exited before control became ready"
            );
            std::thread::sleep(Duration::from_millis(20));
        };
        let mut agent =
            AgentClient::connect(&path.join("agent.sock"), Duration::from_secs(120)).unwrap();
        let exec = |agent: &mut AgentClient, argv: &[&str]| {
            let output = agent.exec(argv, Duration::from_secs(120)).unwrap();
            assert_eq!(
                output.exit_code,
                0,
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
            output.stdout
        };
        let kernel = exec(
            &mut agent,
            &[
                "sh",
                "-ec",
                "dmesg | grep -iE 'virgl|cap.?set' || { mount -t debugfs debugfs /sys/kernel/debug 2>/dev/null || true; cat /sys/kernel/debug/dri/*/virtio-gpu-features; }",
            ],
        );
        eprintln!("guest GPU: {}", String::from_utf8_lossy(&kernel));
        if mode == "venus" {
            assert!(
                String::from_utf8_lossy(&kernel).contains("id 4"),
                "guest did not detect the Venus capset"
            );
        }
        let script = include_str!("guest_gpu.py");
        exec(
            &mut agent,
            &[
                "python3",
                "-c",
                "import sys; open('/run/gpu-probe.py','w').write(sys.argv[1])",
                script,
            ],
        );
        exec(
            &mut agent,
            &[
                "sh",
                "-ec",
                "python3 /run/gpu-probe.py >/run/gpu-probe.log 2>&1 &",
            ],
        );
        let renderer = exec(
            &mut agent,
            &[
                "sh",
                "-ec",
                "for n in $(seq 1 200); do test -f /run/gpu-scanout.json && { cat /run/gpu-scanout.json; exit 0; }; sleep .1; done; cat /run/gpu-probe.log >&2; exit 1",
            ],
        );
        assert!(
            String::from_utf8_lossy(&renderer)
                .to_lowercase()
                .contains("virgl")
        );
        let reply = control
            .call(Ctrl::SnapshotSave { name: "gpu".into() })
            .unwrap();
        assert!(
            matches!(
                reply,
                Outcome::Error {
                    code: vmlab_hv_proto::control::ErrorCode::Unsupported,
                    ..
                }
            ),
            "snapshot unexpectedly succeeded: {reply:?}"
        );
        assert!(matches!(
            control
                .call(Ctrl::SnapshotLoad { name: "gpu".into() })
                .unwrap(),
            Outcome::Error {
                code: vmlab_hv_proto::control::ErrorCode::Unsupported,
                ..
            }
        ));
        let display = DisplayClient::connect(
            path.join("display.sock"),
            Capabilities(Capabilities::FRAMES | Capabilities::CPU_COPY | Capabilities::DMABUF_OK),
        )
        .unwrap();
        display
            .stream()
            .set_read_timeout(Some(Duration::from_secs(15)))
            .unwrap();
        let mut buffers = Vec::new();
        let mut geometry = (0, 0, 0, SurfaceKind::Dmabuf);
        let mut requested = false;
        loop {
            let packet = display.receive().unwrap();
            match packet.message {
                Message::SurfaceConfigure {
                    width,
                    height,
                    stride,
                    kind,
                    ..
                } => {
                    geometry = (width, height, stride, kind);
                    buffers = packet.fds;
                }
                Message::FrameReady {
                    surface_id,
                    buffer,
                    seq,
                    ..
                } => {
                    if geometry.3 == SurfaceKind::Dmabuf && !requested {
                        display.cpu_copy(surface_id).unwrap();
                        requested = true;
                    }
                    if geometry.3 == SurfaceKind::Shm && requested {
                        assert_eq!((geometry.0, geometry.1), (1024, 768));
                        let file = File::from(buffers.remove(buffer as usize));
                        let mut pixel = [0; 4];
                        file.read_exact_at(&mut pixel, 300 * geometry.2 as u64 + 400 * 4)
                            .unwrap();
                        for (actual, expected) in pixel[..3].iter().zip([26u8, 51, 178]) {
                            assert!(
                                actual.abs_diff(expected) <= 2,
                                "GL screenshot pixel {pixel:?}"
                            );
                        }
                        display.release(surface_id, buffer, seq).unwrap();
                        break;
                    }
                    display.release(surface_id, buffer, seq).unwrap();
                }
                other => panic!("unexpected display message: {other:?}"),
            }
        }
        control.call(Ctrl::Quit).unwrap();
        assert!(child.wait().unwrap().success());
    }));
    if result.is_err() {
        eprintln!(
            "GPU S1 hypervisor: {}\nserial: {}",
            std::fs::read_to_string(path.join("hv.log")).unwrap_or_default(),
            std::fs::read_to_string(path.join("serial.log"))
                .unwrap_or_default()
                .lines()
                .rev()
                .take(30)
                .collect::<Vec<_>>()
                .into_iter()
                .rev()
                .collect::<Vec<_>>()
                .join("\n")
        );
    }
    result.unwrap();
}
