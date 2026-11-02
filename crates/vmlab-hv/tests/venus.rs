//! S1: a Venus Vulkan device and Vulkan-rendered native display frames.

use serde_json::json;
use std::fs::File;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};
use vmlab_hv_display::{Capabilities, Client as DisplayClient, FrameFlags, Message, SurfaceKind};
use vmlab_hv_proto::{client::Client, control::Command as Ctrl};
use vmlab_hv_tests::{agent::AgentClient, process::ManagedChild};

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
            .env("VMLAB_HV_LOG", "vmlab_hv_devices::virtio::gpu=debug,info")
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
                Capabilities::FRAMES | Capabilities::DMABUF_OK | Capabilities::FRAME_FLAGS,
            ),
        )
        .unwrap();
        display
            .stream()
            .set_read_timeout(Some(Duration::from_secs(30)))
            .unwrap();
        exec(
            &mut agent,
            "XDG_RUNTIME_DIR=/run/venus-s1 VK_DRIVER_FILES=/usr/share/vulkan/icd.d/virtio_icd.x86_64.json seatd-launch -- gamescope --backend drm --expose-wayland -- sh -c 'DISABLE_GAMESCOPE_WSI=1 vkcube --wsi wayland --c 120; echo $? >/run/venus-s1/cube.status' >/run/venus-s1/render.log 2>&1 &",
        );
        let mut frames = 0;
        let mut dma = false;
        let deadline = Instant::now() + Duration::from_secs(60);
        while frames < 5 && Instant::now() < deadline {
            let packet = display.receive().unwrap_or_else(|error| {
                let render = exec(&mut agent, "cat /run/venus-s1/render.log");
                panic!("waiting for Vulkan frames: {error}; guest renderer:\n{render}");
            });
            match packet.message {
                Message::SurfaceConfigure { kind, .. } => dma = kind == SurfaceKind::Dmabuf,
                Message::FrameReady {
                    surface_id,
                    buffer,
                    seq,
                    flags,
                    ..
                } => {
                    display.release(surface_id, buffer, seq).unwrap();
                    if dma && !flags.has(FrameFlags::PLACEHOLDER) {
                        frames += 1;
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
        assert!(frames >= 5, "Vulkan app did not publish frames: {render}");
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
