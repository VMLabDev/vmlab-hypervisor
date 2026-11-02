//! S1: a Venus Vulkan device and Vulkan-rendered native display frames.

use serde_json::json;
use std::fs::File;
use std::os::unix::fs::FileExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};
use vmlab_hv_display::{Capabilities, Client as DisplayClient, FrameFlags, Message, SurfaceKind};
use vmlab_hv_proto::{client::Client, control::Command as Ctrl};
use vmlab_hv_tests::{agent::AgentClient, process::ManagedChild};

#[path = "venus/windows.rs"]
mod windows;

#[test]
fn steamos_venus_vulkan_device_and_frames() {
    let Some(node) = std::env::var_os("VMLAB_HV_TEST_VENUS_RENDER_NODE") else {
        eprintln!("SKIP Venus S1: set VMLAB_HV_TEST_VENUS_RENDER_NODE to a DRM render node");
        return;
    };
    let node = PathBuf::from(node);
    let home = PathBuf::from(std::env::var_os("HOME").unwrap());
    let template = std::env::var_os("VMLAB_HV_TEST_VENUS_DISK")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            home.join(".local/share/vmlab/templates/x86_64/steamos/3.8.14.3/disk.qcow2")
        });
    let firmware = home.join(".local/share/vmlab/guest/firmware/x86_64");
    if !Path::new("/dev/kvm").exists()
        || !template.is_file()
        || !firmware.join("OVMF_CODE_4M.fd").is_file()
        || !firmware.join("OVMF_VARS_4M.fd").is_file()
    {
        eprintln!("SKIP Venus S1: KVM, SteamOS template or OVMF missing");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path();
    // Other worktrees may rebuild the shared target while this test waits for a VM slot.
    let binary = path.join("vmlab-hv");
    std::fs::copy(env!("CARGO_BIN_EXE_vmlab-hv"), &binary).unwrap();
    let disk = path.join("disk.qcow2");
    vmlab_disk::ops::create_linked_clone(&template, &disk).unwrap();
    let vars = path.join("vars.fd");
    std::fs::copy(firmware.join("OVMF_VARS_4M.fd"), &vars).unwrap();
    let machine = json!({
        "name": "venus-s1", "machine": "q35", "arch": "x86_64", "accel": "kvm",
        "vcpus": 2, "memory_mib": 4096,
        "boot": {"kind": "firmware", "firmware": "ovmf", "code": firmware.join("OVMF_CODE_4M.fd"), "vars": vars, "secure_boot": false},
        "console_pci": {"bus": 0, "device": 1, "function": 0},
        "display": {"device": "virtio-vga", "pci": {"bus": 0, "device": 3, "function": 0}, "gpu": "venus", "render_node": node},
        "disks": [{"id": "root", "path": disk, "format": "qcow2", "read_only": false, "vmstate": true, "pci": {"bus": 0, "device": 2, "function": 0}}],
        "nics": [], "serial_log": path.join("serial.log"),
        "ports": [{"name": "vmlab.agent.0", "socket": path.join("agent.sock")}]
    });
    std::fs::write(
        path.join("machine.json"),
        serde_json::to_vec(&machine).unwrap(),
    )
    .unwrap();
    let log = File::create(path.join("hv.log")).unwrap();
    let mut child = ManagedChild::spawn(
        Command::new(binary)
            .arg("--machine")
            .arg(path.join("machine.json"))
            .arg("--control")
            .arg(path.join("control.sock"))
            .env(
                "VMLAB_HV_LOG",
                "info,vmlab_hv_devices::virtio::gpu::renderer=debug",
            )
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
                "Venus VM exited before control became ready"
            );
            std::thread::sleep(Duration::from_millis(20));
        };
        let mut agent =
            AgentClient::connect(&path.join("agent.sock"), Duration::from_secs(120)).unwrap();
        let exec = |agent: &mut AgentClient, script: &str| {
            let output = agent
                .exec(&["sh", "-ec", script], Duration::from_secs(120))
                .unwrap();
            assert_eq!(
                output.exit_code,
                0,
                "stdout: {}\nstderr: {}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            String::from_utf8(output.stdout).unwrap()
        };
        for cycle in 0..2 {
            exec(
                &mut agent,
                "systemctl stop sddm.service; mkdir -p /run/venus-s1; chmod 700 /run/venus-s1",
            );
            let devices = exec(
                &mut agent,
                "XDG_RUNTIME_DIR=/run/venus-s1 VK_DRIVER_FILES=/usr/share/vulkan/icd.d/virtio_icd.x86_64.json vulkaninfo --summary",
            );
            eprintln!("Venus on {}:\n{devices}", Path::new(&node).display());
            assert!(
                devices.contains("DRIVER_ID_MESA_VENUS"),
                "not a Venus Vulkan device: {devices}"
            );
            let display = DisplayClient::connect(
                path.join("display.sock"),
                Capabilities(
                    Capabilities::FRAMES
                        | Capabilities::DMABUF_OK
                        | Capabilities::FRAME_FLAGS
                        | Capabilities::CPU_COPY
                        | Capabilities::INPUT,
                ),
            )
            .unwrap();
            display
                .stream()
                .set_read_timeout(Some(Duration::from_secs(30)))
                .unwrap();
            display
                .send(&Message::RequestSize {
                    width: 1024,
                    height: 768,
                })
                .unwrap();
            exec(
                &mut agent,
                "XDG_RUNTIME_DIR=/run/venus-s1 VK_DRIVER_FILES=/usr/share/vulkan/icd.d/virtio_icd.x86_64.json seatd-launch -- gamescope --backend drm --expose-wayland -- sh -c 'DISABLE_GAMESCOPE_WSI=1 vkcube --wsi wayland --c 120; echo $? >/run/venus-s1/cube.status' >/run/venus-s1/render.log 2>&1 &",
            );
            let mut frames = 0;
            let mut dma = false;
            let mut buffers = Vec::new();
            let mut dimensions = (0, 0, 0);
            let mut copy_requested = false;
            let mut rendered_pixels = false;
            let deadline = Instant::now() + Duration::from_secs(60);
            while (!rendered_pixels || frames < 20) && Instant::now() < deadline {
                let packet = display.receive().unwrap_or_else(|error| {
                    let render = exec(&mut agent, "cat /run/venus-s1/render.log");
                    panic!("waiting for Vulkan frames: {error}; guest renderer:\n{render}");
                });
                match packet.message {
                    Message::SurfaceConfigure {
                        kind,
                        width,
                        height,
                        stride,
                        ..
                    } => {
                        dma = kind == SurfaceKind::Dmabuf;
                        buffers = packet.fds;
                        dimensions = (width, height, stride);
                    }
                    Message::FrameReady {
                        surface_id,
                        buffer,
                        seq,
                        flags,
                        ..
                    } => {
                        if !dma && copy_requested && !flags.has(FrameFlags::PLACEHOLDER) {
                            let file = File::from(buffers[buffer as usize].try_clone().unwrap());
                            let mut pixels = vec![0; dimensions.2 as usize * dimensions.1 as usize];
                            file.read_exact_at(&mut pixels, 0).unwrap();
                            let colours: std::collections::HashSet<_> = pixels
                                .as_chunks::<4>()
                                .0
                                .iter()
                                .step_by(16)
                                .map(|pixel| [pixel[0], pixel[1], pixel[2]])
                                .collect();
                            rendered_pixels = colours.len() > 256;
                            if rendered_pixels {
                                assert_eq!((dimensions.0, dimensions.1), (1024, 768));
                                let sample = |x: u32, y: u32| {
                                    let offset = (y * dimensions.2 + x * 4) as usize;
                                    [pixels[offset], pixels[offset + 1], pixels[offset + 2]]
                                };
                                let background = sample(dimensions.0 / 2, 16);
                                for y in 16..dimensions.1 / 8 {
                                    for x in (dimensions.0 / 3..dimensions.0 * 2 / 3).step_by(8) {
                                        let actual = sample(x, y);
                                        assert!(
                                            actual
                                                .iter()
                                                .zip(background)
                                                .all(|(a, b)| a.abs_diff(b) <= 2),
                                            "Vulkan scanout layout corrupted at ({x},{y}): {actual:?}, background={background:?}"
                                        );
                                    }
                                }
                            }
                            if rendered_pixels
                                && let Some(output) = std::env::var_os("VMLAB_HV_TEST_VENUS_FRAME")
                            {
                                let mut ppm =
                                    format!("P6\n{} {}\n255\n", dimensions.0, dimensions.1)
                                        .into_bytes();
                                for row in pixels.chunks_exact(dimensions.2 as usize) {
                                    for pixel in row[..dimensions.0 as usize * 4].as_chunks::<4>().0
                                    {
                                        ppm.extend_from_slice(&[pixel[2], pixel[1], pixel[0]]);
                                    }
                                }
                                std::fs::write(output, ppm).unwrap();
                            }
                        }
                        display.release(surface_id, buffer, seq).unwrap();
                        if dma && !flags.has(FrameFlags::PLACEHOLDER) {
                            frames += 1;
                            if frames >= 20 && !copy_requested {
                                display.cpu_copy(surface_id).unwrap();
                                copy_requested = true;
                            }
                        }
                    }
                    _ => {}
                }
            }
            drop(display);
            let render = exec(
                &mut agent,
                "for n in $(seq 1 300); do test ! -f /run/venus-s1/cube.status || { cat /run/venus-s1/render.log; test $(cat /run/venus-s1/cube.status) = 0; exit; }; sleep .1; done; cat /run/venus-s1/render.log >&2; exit 1",
            );
            eprintln!("Vulkan render: {render}");
            assert!(
                frames >= 20 && rendered_pixels,
                "Vulkan scene was not received: {render}"
            );
            exec(
                &mut agent,
                "python3 -c 'import concurrent.futures, subprocess; pool=concurrent.futures.ThreadPoolExecutor(max_workers=4); results=list(pool.map(lambda _: subprocess.run([\"vulkaninfo\",\"--summary\"], stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL).returncode, range(24))); assert not any(results), results'",
            );
            let errors = exec(&mut agent, "dmesg | grep 'response 0x12' || true");
            assert!(errors.is_empty(), "guest GPU command errors:\n{errors}");
            let renderer_log = std::fs::read_to_string(path.join("hv.log")).unwrap();
            assert!(
                !renderer_log.contains("invalid context id")
                    && !renderer_log.contains("GPU command failed:"),
                "renderer GPU command error:\n{renderer_log}"
            );
            if cycle == 0 {
                let pid = child.id();
                control.call(Ctrl::Status).unwrap();
                control.take_events();
                agent.shutdown("reboot", Duration::from_secs(20)).unwrap();
                drop(agent);
                let deadline = Instant::now() + Duration::from_secs(120);
                loop {
                    let event = control
                        .wait_event_timeout(deadline.saturating_duration_since(Instant::now()))
                        .unwrap()
                        .expect("guest reset did not arrive");
                    if event == vmlab_hv_proto::control::Event::Reset {
                        break;
                    }
                    assert!(Instant::now() < deadline, "guest reset timed out");
                }
                assert_eq!(child.id(), pid);
                assert!(child.try_wait().unwrap().is_none());
                agent = AgentClient::connect(&path.join("agent.sock"), Duration::from_secs(120))
                    .unwrap();
            }
        }
        control.call(Ctrl::Quit).unwrap();
        assert!(child.wait().unwrap().success());
    }));
    if result.is_err() {
        eprintln!(
            "Venus S1 hypervisor:\n{}\nserial:\n{}",
            std::fs::read_to_string(path.join("hv.log")).unwrap_or_default(),
            std::fs::read_to_string(path.join("serial.log")).unwrap_or_default()
        );
    }
    result.unwrap();
}
