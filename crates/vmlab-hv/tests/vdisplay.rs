//! S1: Linux publishes BAR2 frames to native and VNC clients without a host GPU.

use std::fs::File;
use std::io::{Read, Write};
use std::os::unix::{fs::FileExt, net::UnixStream};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::json;
use vmlab_hv_display::{Capabilities, Client as DisplayClient, Message, SurfaceKind};
use vmlab_hv_proto::{
    client::Client,
    control::{Command as Ctrl, Outcome},
};
use vmlab_hv_tests::{agent::AgentClient, process::ManagedChild};

fn rfb_pattern(path: PathBuf) {
    fn read<const N: usize>(s: &mut UnixStream) -> [u8; N] {
        let mut bytes = [0; N];
        s.read_exact(&mut bytes).unwrap();
        bytes
    }
    let mut stream = UnixStream::connect(path).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(15)))
        .unwrap();
    assert_eq!(&read::<12>(&mut stream), b"RFB 003.008\n");
    stream.write_all(b"RFB 003.008\n").unwrap();
    assert_eq!(read::<2>(&mut stream), [1, 1]);
    stream.write_all(&[1]).unwrap();
    assert_eq!(read::<4>(&mut stream), [0; 4]);
    stream.write_all(&[1]).unwrap();
    let geometry = read::<4>(&mut stream);
    assert_eq!(geometry, [0, 80, 0, 48]);
    let _format = read::<16>(&mut stream);
    let length = u32::from_be_bytes(read::<4>(&mut stream));
    let mut name = vec![0; length as usize];
    stream.read_exact(&mut name).unwrap();
    stream.write_all(&[3, 0, 0, 0, 0, 0, 0, 80, 0, 48]).unwrap();
    assert_eq!(read::<4>(&mut stream), [0, 0, 0, 1]);
    let rectangle = read::<12>(&mut stream);
    assert_eq!(&rectangle[4..], &[0, 80, 0, 48, 0, 0, 0, 0]);
    let mut pixels = vec![0; 80 * 48 * 4];
    stream.read_exact(&mut pixels).unwrap();
    assert_eq!(pixels, [0x21, 0x65, 0xa9, 0].repeat(80 * 48));
    println!("VNC: 80x48 test pattern verified");
}

#[test]
#[cfg_attr(vmlab_ci_no_kvm, ignore = "requires KVM and a Linux guest asset")]
fn linux_bar2_publisher_native_vnc_and_stale_fallback() {
    let Some(template) = std::env::var_os("VMLAB_HV_TEST_VDISPLAY_DISK") else {
        eprintln!(
            "SKIP virtual display S1: set VMLAB_HV_TEST_VDISPLAY_DISK to a Linux template with Python 3, libatomic and vmlab-agent"
        );
        return;
    };
    if !std::path::Path::new("/dev/kvm").exists() {
        eprintln!("SKIP virtual display S1: /dev/kvm missing");
        return;
    }
    let dir = tempfile::Builder::new()
        .prefix("vd-")
        .tempdir_in("/tmp")
        .unwrap();
    let path = dir.path();
    let firmware = PathBuf::from(std::env::var_os("HOME").unwrap())
        .join(".local/share/vmlab/guest/firmware/x86_64");
    let disk_dir = tempfile::tempdir().unwrap();
    let disk = disk_dir.path().join("disk.qcow2");
    vmlab_disk::ops::create_linked_clone(&PathBuf::from(template), &disk).unwrap();
    let vars = path.join("vars.fd");
    std::fs::copy(firmware.join("OVMF_VARS_4M.fd"), &vars).unwrap();
    let machine = json!({
        "name":"vdisplay-s1", "machine":"q35", "arch":"x86_64", "accel":"kvm", "vcpus":2, "memory_mib":2048,
        "boot":{"kind":"firmware", "firmware":"ovmf", "code":firmware.join("OVMF_CODE_4M.fd"), "vars":vars},
        "console_pci":{"bus":0,"device":1,"function":0},
        "display":{"device":"virtio-vga", "pci":{"bus":0,"device":3,"function":0}},
        "virtual_display":{"pci":{"bus":0,"device":4,"function":0}},
        "disks":[{"id":"root", "path":disk, "format":"qcow2", "read_only":false, "vmstate":true, "pci":{"bus":0,"device":2,"function":0}}],
        "nics":[], "serial_log":path.join("serial.log"),
        "ports":[{"name":"vmlab.agent.0", "socket":path.join("agent.sock")}]
    });
    std::fs::write(
        path.join("machine.json"),
        serde_json::to_vec(&machine).unwrap(),
    )
    .unwrap();
    let mut child = ManagedChild::spawn(Command::new("systemd-run")
        .args(["--user", "--scope", "-q", "-p", "MemoryMax=24G", "-p", "MemorySwapMax=0", "--"])
        .args(["sh", "-c", "export XDG_RUNTIME_DIR=\"$1\" XDG_STATE_HOME=\"$1\" VMLAB_SKIP_UPDATES=1; exec \"$2\" --machine \"$3\" --control \"$4\"", "sh"])
        .arg(path).arg(env!("CARGO_BIN_EXE_vmlab-hv"))
        .arg(path.join("machine.json"))
        .arg(path.join("control.sock"))
        .stdout(Stdio::null()).stderr(File::create(path.join("hv.log")).unwrap())).unwrap();
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let deadline = Instant::now() + Duration::from_secs(30);
        let mut control = loop {
            if let Ok(client) = Client::connect(path.join("control.sock")) {
                break client;
            }
            assert!(
                Instant::now() < deadline && child.try_wait().unwrap().is_none(),
                "launch failed: {}",
                std::fs::read_to_string(path.join("hv.log")).unwrap()
            );
            std::thread::sleep(Duration::from_millis(20));
        };
        assert!(matches!(
            control.call(Ctrl::Status).unwrap(),
            Outcome::Ok { .. }
        ));
        let mut agent =
            AgentClient::connect(&path.join("agent.sock"), Duration::from_secs(120)).unwrap();
        let client = DisplayClient::connect(
            path.join("display.sock"),
            Capabilities(Capabilities::FRAMES | Capabilities::INPUT | Capabilities::CURSOR),
        )
        .unwrap();
        client
            .stream()
            .set_read_timeout(Some(Duration::from_secs(20)))
            .unwrap();
        client
            .send(&Message::RequestSize {
                width: 9000,
                height: 1200,
            })
            .unwrap();
        let publisher = std::thread::spawn(move || {
            agent
                .exec(
                    &["python3", "-c", include_str!("guest_vdisplay.py")],
                    Duration::from_secs(20),
                )
                .unwrap()
        });
        let mut dimensions = (0, 0, 0);
        let mut buffers = Vec::new();
        let mut shared_surface = None;
        let mut frames = 0;
        let mut cursor = false;
        let mut vnc = None;
        let deadline = Instant::now() + Duration::from_secs(25);
        loop {
            assert!(
                Instant::now() < deadline,
                "timed out waiting for shared frames and stale fallback"
            );
            let packet = client.receive().unwrap();
            match packet.message {
                Message::SurfaceConfigure {
                    surface_id,
                    width,
                    height,
                    stride,
                    kind: SurfaceKind::Shm,
                    ..
                } => {
                    dimensions = (width, height, stride);
                    buffers = packet.fds;
                    if (width, height) == (80, 48) {
                        assert!(
                            shared_surface.replace(surface_id).is_none(),
                            "steady virtual display reconfigured"
                        );
                    } else if shared_surface.is_some() {
                        break;
                    }
                }
                Message::FrameReady {
                    surface_id,
                    buffer,
                    seq,
                    ..
                } => {
                    if dimensions.0 == 80 {
                        let mut pixels = vec![0; (dimensions.2 * dimensions.1) as usize];
                        File::from(buffers[buffer as usize].try_clone().unwrap())
                            .read_exact_at(&mut pixels, 0)
                            .unwrap();
                        assert_eq!(pixels, [0x21, 0x65, 0xa9, 0].repeat(80 * 48));
                        frames += 1;
                        if vnc.is_none() {
                            let socket = path.join("vnc.sock");
                            vnc = Some(std::thread::spawn(move || rfb_pattern(socket)));
                        }
                    }
                    client.release(surface_id, buffer, seq).unwrap();
                }
                Message::CursorShape {
                    width: 1,
                    height: 1,
                    argb,
                    ..
                } => {
                    assert_eq!(argb, [3, 4, 5, 255]);
                    cursor = true;
                }
                _ => {}
            }
        }
        let published = publisher.join().unwrap();
        if published.exit_code != 0 {
            let mut diagnostics =
                AgentClient::connect(&path.join("agent.sock"), Duration::from_secs(10)).unwrap();
            let details = diagnostics
                .exec(&["sh", "-c", "dmesg | tail -25"], Duration::from_secs(10))
                .unwrap();
            eprintln!(
                "guest publisher failure: stdout={} stderr={} dmesg={}",
                String::from_utf8_lossy(&published.stdout),
                String::from_utf8_lossy(&published.stderr),
                String::from_utf8_lossy(&details.stdout)
            );
        }
        assert_eq!(
            published.exit_code,
            0,
            "{}",
            String::from_utf8_lossy(&published.stderr)
        );
        let report: serde_json::Value = serde_json::from_slice(&published.stdout).unwrap();
        assert_eq!(report["request"], json!([3840, 1200, 1]));
        assert!(frames >= 3 && cursor, "frames={frames}, cursor={cursor}");
        vnc.expect("VNC was not connected").join().unwrap();
        println!(
            "native: {frames} frames, one configure, cursor, resize and stale fallback verified; guest: {report}"
        );
        let mut agent =
            AgentClient::connect(&path.join("agent.sock"), Duration::from_secs(20)).unwrap();
        let pci = report["pci"].as_str().unwrap();
        let header = |agent: &mut AgentClient, mutation: &str| {
            let script = r#"import json,mmap,struct,sys
with open('/sys/bus/pci/devices/'+sys.argv[1]+'/resource2','r+b',buffering=0) as resource:
    shared=mmap.mmap(resource.fileno(),128*1024*1024)
    base=struct.unpack_from('<Q',shared,0x28)[0]
    step=struct.unpack_from('<I',shared,0x24)[0]
    buffer=struct.unpack_from('<I',shared,0x114)[0]
    offset=base+step*buffer
    if sys.argv[2]:
        struct.pack_into('<I',shared,0x104,int(sys.argv[2]))
        shared[offset:offset+4]=b'\x63'*4
    print(json.dumps({'version':struct.unpack_from('<I',shared,0x104)[0],
        'request':struct.unpack_from('<III',shared,0x34),'pixel':list(shared[offset:offset+4])}))
    shared.close()
"#;
            let output = agent
                .exec(
                    &["python3", "-c", script, pci, mutation],
                    Duration::from_secs(20),
                )
                .unwrap();
            assert_eq!(
                output.exit_code,
                0,
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
            serde_json::from_slice::<serde_json::Value>(&output.stdout).unwrap()
        };
        let saved = header(&mut agent, "");
        let outcome = control
            .call(Ctrl::SnapshotSave {
                name: "vdisplay".into(),
            })
            .unwrap();
        assert!(matches!(outcome, Outcome::Ok { .. }), "save: {outcome:?}");
        assert_eq!(header(&mut agent, "2222")["version"], 2222);
        let outcome = control
            .call(Ctrl::SnapshotLoad {
                name: "vdisplay".into(),
            })
            .unwrap();
        assert!(matches!(outcome, Outcome::Ok { .. }), "load: {outcome:?}");
        agent.resync(Duration::from_secs(20)).unwrap();
        let restored = header(&mut agent, "");
        assert_eq!(restored["version"], saved["version"]);
        assert_eq!(restored["request"], saved["request"]);
        assert_eq!(
            restored["pixel"],
            json!([99, 99, 99, 99]),
            "BAR2 pixels must not be restored"
        );
        println!("S1 snapshot: header restored, pixels excluded");
        assert!(matches!(
            control.call(Ctrl::Quit).unwrap(),
            Outcome::Ok { .. }
        ));
    }));
    if result.is_err() {
        eprintln!(
            "hypervisor: {}\nserial: {}",
            std::fs::read_to_string(path.join("hv.log")).unwrap_or_default(),
            std::fs::read_to_string(path.join("serial.log")).unwrap_or_default()
        );
    }
    let _ = child.kill();
    let _ = child.wait();
    if let Err(error) = result {
        std::panic::resume_unwind(error);
    }
}
