//! The headless viewer drives a real firmware guest through S1 and the agent.

use std::net::Shutdown;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use vmlab_hv_display::Message;
use vmlab_hv_proto::{
    client::Client,
    control::{Command as Control, Outcome},
};
use vmlab_hv_tests::{agent::AgentClient, process::ManagedChild};
use vmlab_hv_viewer::{Core, input::pointer};

struct Observer {
    core: Arc<Mutex<Core>>,
    client: Arc<vmlab_hv_display::Client>,
    reader: Option<std::thread::JoinHandle<()>>,
}
impl Observer {
    fn new(path: &Path) -> Self {
        let core = Core::connect(path, true, "ctrl+alt").unwrap();
        let client = core.client.clone();
        let source = client.clone();
        let core = Arc::new(Mutex::new(core));
        let state = core.clone();
        let reader = std::thread::spawn(move || {
            while let Ok(packet) = source.receive() {
                let mut core = state.lock().unwrap();
                if let Some(frame) = core.receive(packet).unwrap() {
                    core.release(&frame).unwrap();
                }
            }
        });
        Self {
            core,
            client,
            reader: Some(reader),
        }
    }
    fn wait(&self, condition: impl Fn(&Core) -> bool) {
        let deadline = Instant::now() + Duration::from_secs(20);
        while !condition(&self.core.lock().unwrap()) {
            assert!(Instant::now() < deadline, "viewer observation timed out");
            std::thread::sleep(Duration::from_millis(20));
        }
    }
}
impl Drop for Observer {
    fn drop(&mut self) {
        self.client.stream().shutdown(Shutdown::Both).unwrap();
        if let Some(reader) = self.reader.take() {
            reader.join().unwrap();
        }
    }
}

fn guest(agent: &mut AgentClient, args: &[&str]) -> Vec<u8> {
    let output = agent.exec(args, Duration::from_secs(90)).unwrap();
    assert_eq!(
        output.exit_code,
        0,
        "guest {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    output.stdout
}
fn quote(text: &str) -> String {
    format!("'{}'", text.replace('\'', "'\\''"))
}

fn template() -> Option<PathBuf> {
    if let Some(path) = std::env::var_os("VMLAB_HV_UBUNTU_26_04") {
        return Some(path.into());
    }
    let root = PathBuf::from(std::env::var_os("HOME")?)
        .join(".local/share/vmlab/templates/x86_64/ubuntu-26.04");
    let mut disks: Vec<_> = std::fs::read_dir(root)
        .ok()?
        .filter_map(Result::ok)
        .map(|e| e.path().join("disk.qcow2"))
        .filter(|p| p.is_file())
        .collect();
    disks.sort();
    disks.pop()
}

#[test]
fn viewer_input_and_resize_reach_guest_via_s1() {
    if !Path::new("/dev/kvm").exists() {
        eprintln!("SKIP viewer S1: /dev/kvm unavailable");
        return;
    }
    let Some(template) = template() else {
        eprintln!("SKIP viewer S1: Ubuntu template absent; set VMLAB_HV_UBUNTU_26_04");
        return;
    };
    for path in [
        "/usr/share/qemu/bios-256k.bin",
        "/usr/share/qemu/vgabios-virtio.bin",
    ] {
        if !Path::new(path).is_file() {
            eprintln!("SKIP viewer S1: {path} unavailable");
            return;
        }
    }
    assert!(
        template.is_file(),
        "missing template {}",
        template.display()
    );
    let directory = tempfile::Builder::new()
        .prefix("hv-viewer-s1-")
        .tempdir_in("/tmp")
        .unwrap();
    let disk = directory.path().join("disk.qcow2");
    vmlab_disk::ops::create_linked_clone(&template, &disk).unwrap();
    let control = directory.path().join("control.sock");
    let display = directory.path().join("display.sock");
    let agent_path = directory.path().join("agent.sock");
    let serial = directory.path().join("serial.log");
    let stderr = directory.path().join("stderr.log");
    let machine = directory.path().join("machine.json");
    std::fs::write(&machine, serde_json::to_vec(&serde_json::json!({
        "machine":"q35", "name":"viewer-s1", "arch":"x86_64", "accel":"kvm", "vcpus":2, "memory_mib":4096, "usb_keyboard":false,
        "boot":{"kind":"firmware","firmware":"seabios","code":"/usr/share/qemu/bios-256k.bin"},
        "disks":[{"id":"disk0","pci":{"bus":0,"device":2,"function":0},"path":disk,"format":"qcow2","read_only":false,"vmstate":true}],
        "console_pci":{"bus":0,"device":1,"function":0},
        "display":{"device":"virtio-vga","pci":{"bus":0,"device":4,"function":0},"socket":display},
        "tablet":{"bus":0,"device":5,"function":0}, "usb":{"bus":0,"device":5,"function":0},
        "ports":[{"name":"vmlab.agent.0","socket":agent_path}],"nics":[],"serial_log":serial
    })).unwrap()).unwrap();
    let binary = std::env::var_os("VMLAB_HV_BINARY")
        .map(PathBuf::from)
        .unwrap_or_else(vmlab_hv_tests::hv_binary);
    // ManagedChild acquires a host-wide VM slot and rechecks /proc before boot.
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
            .arg(binary)
            .arg("--machine")
            .arg(machine)
            .arg("--control")
            .arg(&control)
            .arg("--paused")
            .stdout(Stdio::null())
            .stderr(std::fs::File::create(&stderr).unwrap()),
    )
    .unwrap();
    let diagnostics = || {
        format!(
            "stderr: {}\nserial: {}",
            std::fs::read_to_string(&stderr).unwrap_or_default(),
            std::fs::read_to_string(&serial).unwrap_or_default()
        )
    };
    let deadline = Instant::now() + Duration::from_secs(20);
    let mut control = loop {
        if let Ok(client) = Client::connect(&control) {
            break client;
        }
        assert!(
            process.try_wait().unwrap().is_none(),
            "guest exited: {}",
            diagnostics()
        );
        assert!(
            Instant::now() < deadline,
            "control timeout: {}",
            diagnostics()
        );
        std::thread::sleep(Duration::from_millis(20));
    };
    let observer = Observer::new(&display);
    assert!(matches!(
        control.call(Control::Resume).unwrap(),
        Outcome::Ok { .. }
    ));
    let mut agent = AgentClient::connect(&agent_path, Duration::from_secs(120))
        .unwrap_or_else(|e| panic!("{e}: {}", diagnostics()));
    observer.wait(|core| core.current.is_some());
    guest(&mut agent, &["systemctl", "stop", "getty@tty1.service"]);
    let repeat = include_str!("guest_repeat.py");
    guest(
        &mut agent,
        &[
            "sh",
            "-c",
            &format!(
                "python3 -c {} >/run/viewer-repeat.log 2>&1 &",
                quote(repeat)
            ),
        ],
    );
    guest(
        &mut agent,
        &[
            "sh",
            "-c",
            "for n in $(seq 1 100); do test -f /run/viewer-repeat-ready && exit 0; sleep .05; done; cat /run/viewer-repeat.log; exit 1",
        ],
    );
    {
        let mut core = observer.core.lock().unwrap();
        core.input.grab();
        for index in 0..3 {
            let messages = core.input.key(30, true);
            core.send(messages).unwrap();
            // Linux atkbd debounces duplicate makes before half its repeat delay.
            std::thread::sleep(if index == 0 {
                Duration::from_secs(1)
            } else {
                Duration::from_millis(50)
            });
        }
        for (code, down) in [(30, false), (28, true), (28, false)] {
            let messages = core.input.key(code, down);
            core.send(messages).unwrap();
        }
    }
    let repeated = guest(
        &mut agent,
        &[
            "python3",
            "-c",
            "import time; from pathlib import Path\np=Path('/run/viewer-repeat.txt')\nfor _ in range(100):\n if p.exists(): break\n time.sleep(.05)\nb=p.read_bytes(); print(repr(b)); assert b == b'aaa\\r', repr(b)",
        ],
    );
    eprintln!(
        "viewer S1 PS/2 console repeat: {}",
        String::from_utf8_lossy(&repeated)
    );
    let script = include_str!("../../vmlab-hv/tests/guest_display.py");
    guest(
        &mut agent,
        &[
            "sh",
            "-c",
            &format!(
                "python3 -c {} record >/run/p3-recorder.log 2>&1 &",
                quote(script)
            ),
        ],
    );
    guest(
        &mut agent,
        &[
            "sh",
            "-c",
            "for n in $(seq 1 100); do test -f /run/p3-input-ready && exit 0; sleep .05; done; cat /run/p3-recorder.log; exit 1",
        ],
    );
    {
        let mut core = observer.core.lock().unwrap();
        core.input.grab();
        for (code, down) in [
            (30, true),
            (30, false),
            (48, true),
            (48, false),
            (58, true),
            (58, false),
            (86, true),
            (86, false),
            (183, true),
            (183, false),
            (115, true),
            (115, false),
        ] {
            let messages = core.input.key(code, down);
            core.send(messages).unwrap();
        }
        core.send(pointer((400., 300.), (801, 601), (801, 601)))
            .unwrap();
        for (bit, down) in [(1, true), (1, false), (2, true), (2, false)] {
            let messages = core.input.button(bit, down);
            core.send(messages).unwrap();
        }
        core.send([Message::Scroll { dx: 0, dy: 1 }]).unwrap();
        let now = Instant::now();
        core.resize(now, 800, 600);
        core.tick(now + Duration::from_millis(200)).unwrap();
    }
    observer.wait(|core| core.leds != 0);
    let output = guest(&mut agent, &["python3", "-c", script, "scanout"]);
    eprintln!("viewer S1 DRM resize: {}", String::from_utf8_lossy(&output));
    observer.wait(|core| {
        core.surfaces
            .values()
            .any(|s| s.width == 800 && s.height == 600)
            && core.cursor.visible
    });
    let check = r#"import json,time
from pathlib import Path
expected=[(1,30,1),(1,30,0),(1,48,1),(1,48,0),(1,86,1),(1,86,0),(1,183,1),(1,183,0),(1,115,1),(1,115,0),(3,0,16383),(3,1,16383),(1,272,1),(1,272,0),(1,274,1),(1,274,0),(2,8,-1)]
for _ in range(100):
 events=[tuple(json.loads(line)[1:]) for line in Path('/run/p3-events.jsonl').read_text().splitlines()]
 if all(event in events for event in expected):
  print(json.dumps({'verified':expected,'received':len(events)})); break
 time.sleep(.05)
else: raise AssertionError({'expected':expected,'received':events})
"#;
    let events = guest(&mut agent, &["python3", "-c", check]);
    eprintln!(
        "viewer S1 guest input: {}",
        String::from_utf8_lossy(&events)
    );
    let before = observer
        .core
        .lock()
        .unwrap()
        .current
        .as_ref()
        .unwrap()
        .surface;
    let pid = process.id();
    guest(
        &mut agent,
        &[
            "sh",
            "-c",
            "nohup sh -c 'sleep 2; reboot' >/dev/null 2>&1 </dev/null &",
        ],
    );
    drop(agent);
    loop {
        match control
            .wait_event()
            .unwrap()
            .expect("control must survive reset")
        {
            vmlab_hv_proto::control::Event::Reset => break,
            vmlab_hv_proto::control::Event::Shutdown { reason } => {
                panic!("reset shut down: {reason:?}")
            }
            _ => {}
        }
    }
    let mut agent = AgentClient::connect(&agent_path, Duration::from_secs(120)).unwrap();
    guest(&mut agent, &["sh", "-c", "echo rebooted"]);
    observer.wait(|core| {
        core.current
            .as_ref()
            .is_some_and(|frame| frame.surface > before)
    });
    assert_eq!(process.id(), pid);
    assert!(process.try_wait().unwrap().is_none());
    assert!(matches!(
        control.call(Control::Status).unwrap(),
        Outcome::Ok { .. }
    ));
    eprintln!("viewer S1: reset retained PID {pid}, control and the original display connection");
    drop(observer);
    assert!(matches!(
        control.call(Control::Quit).unwrap(),
        Outcome::Ok { .. }
    ));
    assert!(process.wait().unwrap().success(), "{}", diagnostics());
}
