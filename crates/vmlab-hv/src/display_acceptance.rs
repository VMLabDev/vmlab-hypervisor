//! P3 observations through native display, RFB, and the real guest agent.

use std::fs::File;
use std::hash::{Hash, Hasher};
use std::io::Write;
use std::net::Shutdown;
use std::os::unix::fs::FileExt;
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use vmlab_hv_display::{Capabilities, Client, Message, SurfaceKind};
use vmlab_hv_tests::agent::AgentClient;

#[derive(Debug, Default)]
struct State {
    frames: u64,
    configures: u64,
    width: u32,
    height: u32,
    hash: u64,
    firmware: bool,
    damage: bool,
    cursor: bool,
    cursor_shape: (u32, u32, u32, u32),
    cursor_position: (i32, i32),
    resize_damage: bool,
    visible: bool,
    leds: u32,
    error: Option<String>,
    damage_areas: Vec<u64>,
    images: Images,
}

#[derive(Clone, Default)]
struct Image {
    width: u32,
    height: u32,
    pixels: Vec<u8>,
}
#[derive(Default)]
struct Images {
    firmware: Vec<Image>,
    latest: Image,
}
impl std::fmt::Debug for Images {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} firmware samples", self.firmware.len())
    }
}

fn recognize(image: &Image) -> String {
    let mut pgm = format!("P5\n{} {}\n255\n", image.width, image.height).into_bytes();
    pgm.extend(
        image
            .pixels
            .as_chunks::<4>()
            .0
            .iter()
            .map(|p| 255 - ((p[0] as u16 + p[1] as u16 + p[2] as u16) / 3) as u8),
    );
    if let Some(directory) = std::env::var_os("VMLAB_HV_DISPLAY_ARTIFACTS") {
        let path = std::path::PathBuf::from(directory)
            .join(format!("frame-{}x{}.pgm", image.width, image.height));
        std::fs::write(path, &pgm).unwrap();
    }
    let mut child = Command::new("tesseract")
        .args(["stdin", "stdout", "--psm", "6"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(&pgm).unwrap();
    let result = child.wait_with_output().unwrap();
    assert!(result.status.success());
    String::from_utf8(result.stdout).unwrap()
}

fn screenshot(path: &Path) -> Image {
    let client = Client::connect(path, Capabilities(Capabilities::FRAMES)).unwrap();
    client
        .stream()
        .set_read_timeout(Some(Duration::from_secs(10)))
        .unwrap();
    let packet = client.receive().unwrap();
    let Message::SurfaceConfigure {
        width,
        height,
        stride,
        kind: SurfaceKind::Shm,
        ..
    } = packet.message
    else {
        panic!("expected screenshot shm configure");
    };
    let Message::FrameReady {
        surface_id,
        buffer,
        seq,
        damage,
        ..
    } = client.receive().unwrap().message
    else {
        panic!("expected first screenshot frame");
    };
    assert!(
        damage
            .iter()
            .any(|r| { r.x == 0 && r.y == 0 && r.width == width && r.height == height })
    );
    let mut pixels = vec![0; stride as usize * height as usize];
    File::from(packet.fds[buffer as usize].try_clone().unwrap())
        .read_exact_at(&mut pixels, 0)
        .unwrap();
    assert_eq!(stride, width * 4);
    client.release(surface_id, buffer, seq).unwrap();
    Image {
        width,
        height,
        pixels,
    }
}

pub(super) fn screenshots_after_unobserved_boot(path: &Path, agent: &mut AgentClient) {
    Observer::guest(
        agent,
        &[
            "sh",
            "-c",
            // Console text becomes visible in vcs1 before fbdev's deferred
            // update reaches the scanout. Keep all display clients absent.
            "for n in $(seq 1 300); do if grep -aq 'login:' /dev/vcs1; then sleep 1; exit 0; fi; sleep .1; done; cat /dev/vcs1; exit 1",
        ],
    );
    let image = screenshot(path);
    assert_eq!((image.width, image.height), (1280, 800));
    let edid = Observer::guest(
        agent,
        &["sh", "-c", "cat /sys/class/drm/card*-Virtual-1/edid"],
    );
    assert_eq!(
        edid,
        include_bytes!("../../../docs/hwref/ubuntu-26.04-display/guest/drm/Virtual-1.edid")
    );
    let text = recognize(&image);
    assert!(
        text.to_lowercase().contains("login:"),
        "first screenshot OCR: {text:?}"
    );
    eprintln!(
        "S1 first screenshot {}x{}: {text:?}",
        image.width, image.height
    );
    Observer::guest(
        agent,
        &[
            "sh",
            "-c",
            // Let fbdev's deferred DRM update reach the scanout without a viewer.
            r"printf '\033[2J\033[HDisconnected console update\n' >/dev/tty1; sleep 1",
        ],
    );
    let image = screenshot(path);
    let text = recognize(&image);
    assert!(
        text.to_lowercase().contains("disconnected console update"),
        "reconnected screenshot OCR: {text:?}"
    );
    eprintln!(
        "S1 reconnected screenshot {}x{}: {text:?}",
        image.width, image.height
    );
}

pub(super) struct Observer {
    client: Arc<Client>,
    state: Arc<(Mutex<State>, Condvar)>,
    reader: Option<JoinHandle<()>>,
}

impl Observer {
    pub(super) fn connect(path: &Path) -> Self {
        let client = Arc::new(Client::connect(path, Capabilities::ALL).unwrap());
        let state = Arc::new((Mutex::new(State::default()), Condvar::new()));
        let (source, destination) = (client.clone(), state.clone());
        let reader = std::thread::spawn(move || {
            let result = Self::receive(&source, &destination);
            if let Err(error) = result {
                destination.0.lock().unwrap().error = Some(error.to_string());
                destination.1.notify_all();
            }
        });
        let observer = Self {
            client,
            state,
            reader: Some(reader),
        };
        observer.wait("initial paused frame", |s| s.frames > 0);
        observer
    }

    fn receive(client: &Client, state: &(Mutex<State>, Condvar)) -> std::io::Result<()> {
        let mut files: Vec<File> = Vec::new();
        let (mut width, mut height, mut stride) = (0, 0, 0);
        loop {
            let packet = client.receive()?;
            match packet.message {
                Message::SurfaceConfigure {
                    width: w,
                    height: h,
                    stride: pitch,
                    kind,
                    ..
                } => {
                    assert_eq!(kind, SurfaceKind::Shm);
                    (width, height, stride) = (w, h, pitch);
                    files = packet.fds.into_iter().map(File::from).collect();
                    state.0.lock().unwrap().configures += 1;
                }
                Message::FrameReady {
                    surface_id,
                    buffer,
                    seq,
                    damage,
                    ..
                } => {
                    let mut pixels = vec![0; stride as usize * height as usize];
                    files[buffer as usize].read_exact_at(&mut pixels, 0)?;
                    let mut hash = std::collections::hash_map::DefaultHasher::new();
                    pixels.hash(&mut hash);
                    let mut s = state.0.lock().unwrap();
                    s.frames += 1;
                    s.damage_areas.push(
                        damage
                            .iter()
                            .map(|r| u64::from(r.width) * u64::from(r.height))
                            .sum(),
                    );
                    s.width = width;
                    s.height = height;
                    let next_hash = hash.finish();
                    if width == 720
                        && height == 400
                        && s.hash != next_hash
                        && pixels.iter().any(|b| *b != 0)
                        && s.images.firmware.len() < 12
                    {
                        s.images.firmware.push(Image {
                            width,
                            height,
                            pixels: pixels.clone(),
                        });
                    }
                    s.hash = next_hash;
                    s.damage |= !damage.is_empty();
                    s.resize_damage |= width == 800
                        && height == 600
                        && damage.iter().any(|r| {
                            r.x == 0 && r.y == 0 && r.width == width && r.height == height
                        });
                    s.firmware |= width == 720 && height == 400 && pixels.iter().any(|b| *b != 0);
                    s.images.latest = Image {
                        width,
                        height,
                        pixels,
                    };
                    drop(s);
                    client.release(surface_id, buffer, seq)?;
                }
                Message::CursorShape {
                    width,
                    height,
                    hot_x,
                    hot_y,
                    argb,
                } => {
                    let mut s = state.0.lock().unwrap();
                    s.cursor |= width > 0 && height > 0 && argb.iter().all(|b| *b == 255);
                    s.cursor_shape = (width, height, hot_x, hot_y);
                }
                Message::CursorState { x, y, visible } => {
                    let mut s = state.0.lock().unwrap();
                    s.visible = visible;
                    s.cursor_position = (x, y);
                }
                Message::LedState { mask } => state.0.lock().unwrap().leds = mask,
                _ => {}
            }
            state.1.notify_all();
        }
    }

    fn wait(&self, stage: &str, ready: impl Fn(&State) -> bool) {
        let deadline = Instant::now() + Duration::from_secs(30);
        let mut state = self.state.0.lock().unwrap();
        while !ready(&state) {
            assert!(state.error.is_none(), "{stage}: {state:?}");
            assert!(Instant::now() < deadline, "{stage} timeout: {state:?}");
            state = self
                .state
                .1
                .wait_timeout(state, Duration::from_millis(100))
                .unwrap()
                .0;
        }
        eprintln!("P3 {stage}: {state:?}");
    }

    fn send(&self, message: Message) {
        self.client.send(&message).unwrap();
    }

    fn guest(agent: &mut AgentClient, args: &[&str]) -> Vec<u8> {
        let output = agent.exec(args, Duration::from_secs(30)).unwrap();
        assert_eq!(
            output.exit_code,
            0,
            "guest command {args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        output.stdout
    }

    pub(super) fn exercise(&self, agent: &mut AgentClient, rfb: &Path) {
        self.wait("firmware frame", |s| s.firmware && s.damage);
        let samples = self.state.0.lock().unwrap().images.firmware.clone();
        let text: Vec<_> = samples.iter().map(recognize).collect();
        assert!(
            text.iter().any(|t| t.to_lowercase().contains("seabios")),
            "firmware OCR: {text:?}"
        );
        eprintln!("P3 recognized SeaBIOS in native frame pixels");
        let old = self.state.0.lock().unwrap().hash;
        let console = Self::guest(
            agent,
            &[
                "sh",
                "-c",
                r"cat /sys/class/graphics/fb0/name; printf '\033[2J\033[HP3 Linux console\n' >/dev/tty1",
            ],
        );
        eprintln!(
            "P3 guest framebuffer: {}",
            String::from_utf8_lossy(&console)
        );
        self.wait("Linux console", |s| s.width >= 800 && s.hash != old);
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let image = self.state.0.lock().unwrap().images.latest.clone();
            let text = recognize(&image);
            if text.to_lowercase().contains("linux console") {
                break;
            }
            assert!(Instant::now() < deadline, "Linux console OCR: {text:?}");
        }
        eprintln!("P3 recognized Linux console text in native frame pixels");
        self.exercise_damage(agent);
        let script = include_str!("../tests/guest_display.py");
        let launch = format!(
            "python3 -c {} record >/run/p3-recorder.log 2>&1 &",
            shell_quote(script)
        );
        Self::guest(agent, &["sh", "-c", &launch]);
        Self::guest(
            agent,
            &[
                "sh",
                "-c",
                "for n in $(seq 1 100); do test -f /run/p3-input-ready && exit 0; sleep .05; done; cat /run/p3-recorder.log; exit 1",
            ],
        );
        for (qcode, down) in [
            ("a", true),
            ("a", false),
            ("caps_lock", true),
            ("caps_lock", false),
        ] {
            self.send(Message::Key {
                qcode: qcode.into(),
                down,
            });
        }
        self.send(Message::PointerAbs {
            x: 400,
            y: 300,
            width: 801,
            height: 601,
        });
        self.send(Message::Buttons { mask: 1 });
        self.send(Message::Buttons { mask: 0 });
        self.send(Message::Buttons { mask: 2 });
        self.send(Message::Buttons { mask: 0 });
        self.send(Message::Scroll { dx: 0, dy: 1 });
        self.wait("keyboard LED", |s| s.leds != 0);
        self.send(Message::RequestSize {
            width: 800,
            height: 600,
        });
        let drm = Self::guest(agent, &["python3", "-c", script, "scanout"]);
        eprintln!("P3 DRM: {}", String::from_utf8_lossy(&drm));
        self.wait("resize and hardware cursor", |s| {
            s.width == 800
                && s.height == 600
                && s.cursor
                && s.visible
                && s.resize_damage
                && s.cursor_shape == (64, 64, 2, 3)
                && s.cursor_position == (123, 234)
        });
        let (configures, hash) = {
            let state = self.state.0.lock().unwrap();
            (state.configures, state.hash)
        };
        Self::guest(agent, &["touch", "/run/p3-switch"]);
        self.wait("same-size scanout replacement", |s| {
            s.configures > configures && s.hash != hash && s.width == 800 && s.height == 600
        });
        let rfb_result = std::process::Command::new("python3")
            .arg("-c")
            .arg(include_str!("../tests/rfb_client.py"))
            .arg(rfb)
            .output()
            .unwrap();
        assert!(
            rfb_result.status.success(),
            "RFB: {}",
            String::from_utf8_lossy(&rfb_result.stderr)
        );
        eprintln!("P3 RFB: {}", String::from_utf8_lossy(&rfb_result.stdout));
        let events = Self::guest(agent, &["python3", "-c", script, "check"]);
        eprintln!("P3 input: {}", String::from_utf8_lossy(&events));
    }

    fn exercise_damage(&self, agent: &mut AgentClient) {
        let edid = Self::guest(
            agent,
            &["sh", "-c", "cat /sys/class/drm/card*-Virtual-1/edid"],
        );
        assert_eq!(
            edid,
            include_bytes!("../../../docs/hwref/ubuntu-26.04-display/guest/drm/Virtual-1.edid")
        );
        let size = Self::guest(agent, &["cat", "/sys/class/graphics/fb0/virtual_size"]);
        assert_eq!(String::from_utf8_lossy(&size).trim(), "1280,800");
        if let Some(root) = std::env::var_os("VMLAB_HV_DISPLAY_HWREF") {
            let root = std::path::PathBuf::from(root).join("guest/drm");
            std::fs::create_dir_all(&root).unwrap();
            std::fs::write(
                root.parent()
                    .unwrap()
                    .parent()
                    .unwrap()
                    .join("display.json"),
                b"{\"kind\":\"display\",\"version\":1}\n",
            )
            .unwrap();
            std::fs::write(root.join("Virtual-1.edid"), &edid).unwrap();
            std::fs::write(root.join("framebuffer-size.txt"), &size).unwrap();
        }
        Self::guest(
            agent,
            &[
                "sh",
                "-c",
                r"systemctl stop getty@tty1; printf '\033[?25l\033[2J\033[HIdle damage check' >/dev/tty1; sleep 2",
            ],
        );
        let before = self.state.0.lock().unwrap().frames;
        std::thread::sleep(Duration::from_secs(2));
        assert_eq!(
            self.state.0.lock().unwrap().frames,
            before,
            "idle console produced frames"
        );
        Self::guest(agent, &["sh", "-c", r"printf '\033[Hj' >/dev/tty1"]);
        self.wait("single-character damage", |s| s.frames > before);
        std::thread::sleep(Duration::from_millis(500));
        {
            let state = self.state.0.lock().unwrap();
            let areas = &state.damage_areas[before as usize..];
            assert!(!areas.is_empty());
            assert!(
                areas.iter().all(|area| *area > 0 && *area <= 4096),
                "character damage: {areas:?}"
            );
        }
        let before = self.state.0.lock().unwrap().frames;
        Self::guest(
            agent,
            &["sh", "-c", r"printf '\033[?25h' >/dev/tty1; sleep 2"],
        );
        self.wait("cursor blink damage", |s| s.frames >= before + 2);
        {
            let state = self.state.0.lock().unwrap();
            let areas = &state.damage_areas[before as usize..];
            assert!(
                areas.iter().all(|area| *area > 0 && *area <= 4096),
                "cursor damage: {areas:?}"
            );
            eprintln!("S1 1280x800, QEMU-identical EDID, idle suppressed; cursor damage {areas:?}");
        }
        Self::guest(
            agent,
            &["sh", "-c", r"printf '\033[?25l' >/dev/tty1; sleep 1"],
        );
    }
}

fn shell_quote(text: &str) -> String {
    format!("'{}'", text.replace('\'', "'\\''"))
}

impl Drop for Observer {
    fn drop(&mut self) {
        let _ = self.client.stream().shutdown(Shutdown::Both);
        if let Some(reader) = self.reader.take() {
            let _ = reader.join();
        }
    }
}

#[test]
fn qemu_ubuntu_display_reference() {
    qemu_display_reference("virtio-vga");
}

#[test]
fn qemu_ubuntu_standard_vga_display_reference() {
    qemu_display_reference("VGA");
}

fn qemu_display_reference(device: &str) {
    use vmlab_hv_tests::process::ManagedChild;
    let Some(template) =
        super::acceptance::template("ubuntu-26.04", "VMLAB_HV_TEMPLATE_UBUNTU_26_04")
    else {
        eprintln!("SKIP QEMU display reference: Ubuntu 26.04 template absent");
        return;
    };
    if !Path::new("/dev/kvm").exists() {
        eprintln!("SKIP QEMU display reference: /dev/kvm absent");
        return;
    }
    if Command::new("qemu-system-x86_64")
        .arg("--version")
        .output()
        .is_err()
    {
        eprintln!("SKIP QEMU display reference: qemu-system-x86_64 absent");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let disk = dir.path().join("disk.qcow2");
    vmlab_disk::ops::create_linked_clone(&template, &disk).unwrap();
    let socket = dir.path().join("agent.sock");
    let mut command = Command::new("qemu-system-x86_64");
    command
        .args([
            "-machine",
            "q35",
            "-accel",
            "kvm",
            "-cpu",
            "host,vmx=off,svm=off",
            "-smp",
            "2",
            "-m",
            "4096M",
            "-display",
            "none",
            "-monitor",
            "none",
            "-serial",
            "none",
            "-device",
            "virtio-serial-pci",
            "-chardev",
        ])
        .arg(format!(
            "socket,id=agent,path={},server=on,wait=off",
            socket.display()
        ))
        .args([
            "-device",
            "virtserialport,chardev=agent,name=vmlab.agent.0",
            "-drive",
        ])
        .arg(format!(
            "file={},format=qcow2,if=none,id=disk",
            disk.display()
        ))
        .args([
            "-device",
            "virtio-blk-pci,drive=disk,bootindex=50",
            "-device",
            device,
        ]);
    let _process = ManagedChild::spawn(&mut command).unwrap();
    let mut agent = AgentClient::connect(&socket, Duration::from_secs(120)).unwrap();
    let edid = Observer::guest(
        &mut agent,
        &["sh", "-c", "cat /sys/class/drm/card*-Virtual-1/edid"],
    );
    eprintln!("QEMU EDID ({} bytes): {edid:02x?}", edid.len());
    if let Some(output) = std::env::var_os("VMLAB_HV_EDID_CAPTURE") {
        std::fs::write(&output, &edid).unwrap();
        let size = Observer::guest(&mut agent, &["cat", "/sys/class/graphics/fb0/virtual_size"]);
        assert_eq!(String::from_utf8_lossy(&size).trim(), "1280,800");
        std::fs::write(
            Path::new(&output).with_file_name("framebuffer-size.txt"),
            size,
        )
        .unwrap();
    }
    assert_eq!(
        edid,
        include_bytes!("../../../docs/hwref/ubuntu-26.04-display/guest/drm/Virtual-1.edid")
    );
}
