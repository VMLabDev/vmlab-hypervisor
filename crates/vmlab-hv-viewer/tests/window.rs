//! A private Xvfb display exercises the executable without touching the desktop.

use std::fs::File;
use std::io::{BufRead, BufReader};
use std::os::fd::AsRawFd;
use std::os::unix::{fs::FileExt, net::UnixListener};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};
use vmlab_hv_display::{FrameFlags, Message, Rect, SurfaceKind, receive, send};
use vmlab_hv_tests::process::ManagedChild;
use x11rb::{
    connection::Connection,
    protocol::{
        xfixes::ConnectionExt as _,
        xproto::{
            AtomEnum, BUTTON_PRESS_EVENT, BUTTON_RELEASE_EVENT, ClientMessageEvent,
            ConfigureWindowAux, ConnectionExt, EventMask, ImageFormat, InputFocus, KEY_PRESS_EVENT,
            KEY_RELEASE_EVENT,
        },
        xtest::ConnectionExt as _,
    },
};

struct XServer(ManagedChild);
impl Drop for XServer {
    fn drop(&mut self) {
        // SAFETY: this PID is the Xvfb child owned by this test. SIGTERM lets
        // Xvfb remove its private display socket and lock before the guard reaps it.
        unsafe {
            libc::kill(self.0.id() as i32, libc::SIGTERM);
        }
        let _ = self.0.wait();
    }
}

#[track_caller]
fn wait(mut condition: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(15);
    while !condition() {
        assert!(
            Instant::now() < deadline,
            "private-display observation timed out"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn executable_presents_letterbox_cursor_input_and_resize_on_private_display() {
    for mode in ["mailbox", "fifo"] {
        exercise(mode);
    }
}

fn exercise(mode: &str) {
    if !Command::new("Xvfb").arg("-help").output().is_ok() {
        eprintln!("SKIP viewer window: Xvfb unavailable");
        return;
    }
    // SAFETY: this probe owns its instance and destroys it before unloading the loader.
    let available = unsafe {
        (|| -> anyhow::Result<bool> {
            let entry = ash::Entry::load()?;
            let instance = entry.create_instance(&ash::vk::InstanceCreateInfo::default(), None)?;
            let devices = instance.enumerate_physical_devices();
            instance.destroy_instance(None);
            Ok(!devices?.is_empty())
        })()
    };
    if !available.unwrap_or(false) {
        eprintln!("SKIP viewer window: no Vulkan physical device");
        return;
    }
    let directory = tempfile::tempdir().unwrap();
    let mut xvfb = XServer(
        ManagedChild::helper(
            Command::new("Xvfb")
                .args([
                    "-displayfd",
                    "1",
                    "-screen",
                    "0",
                    "1024x768x24",
                    "-nolisten",
                    "tcp",
                    "-ac",
                ])
                .stdout(Stdio::piped())
                .stderr(Stdio::null()),
        )
        .unwrap(),
    );
    let mut number = String::new();
    BufReader::new(xvfb.0.stdout.take().unwrap())
        .read_line(&mut number)
        .unwrap();
    assert!(
        number.trim().parse::<u16>().is_ok(),
        "Xvfb did not allocate a private display"
    );
    let display = format!(":{}", number.trim());
    let (connection, screen) = x11rb::connect(Some(&display)).unwrap();
    connection
        .xfixes_query_version(5, 0)
        .unwrap()
        .reply()
        .unwrap();
    let root = connection.setup().roots[screen].root;
    let socket = directory.path().join("display.sock");
    let listener = UnixListener::bind(&socket).unwrap();
    listener.set_nonblocking(true).unwrap();
    let stderr = directory.path().join("viewer.log");
    let mut viewer = ManagedChild::helper(
        Command::new(env!("CARGO_BIN_EXE_vmlab-hv-viewer"))
            .args(["--display"])
            .arg(&socket)
            .env("DISPLAY", &display)
            .env_remove("WAYLAND_DISPLAY")
            .env("WINIT_UNIX_BACKEND", "x11")
            .env("VMLAB_VIEWER_PRESENT_MODE", mode)
            .stdout(Stdio::null())
            .stderr(File::create(&stderr).unwrap()),
    )
    .unwrap();
    let mut stream = None;
    wait(|| {
        if let Ok((client, _)) = listener.accept() {
            stream = Some(client);
            true
        } else {
            assert!(
                viewer.try_wait().unwrap().is_none(),
                "viewer startup: {}",
                std::fs::read_to_string(&stderr).unwrap()
            );
            false
        }
    });
    let stream = stream.unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(15)))
        .unwrap();
    let Message::Hello {
        version,
        capabilities,
    } = receive(&stream).unwrap().message
    else {
        panic!("no Hello")
    };
    send(
        &stream,
        &Message::Hello {
            version,
            capabilities,
        },
        &[],
    )
    .unwrap();
    let file = tempfile::tempfile().unwrap();
    let pixels: Vec<_> = [0, 0, 255, 255]
        .into_iter()
        .cycle()
        .take(64 * 32 * 4)
        .collect();
    file.write_all_at(&pixels, 0).unwrap();
    send(
        &stream,
        &Message::SurfaceConfigure {
            surface_id: 1,
            width: 64,
            height: 32,
            drm_fourcc: u32::from_le_bytes(*b"XR24"),
            stride: 256,
            modifier: 0,
            kind: SurfaceKind::Shm,
            buffers: 1,
        },
        &[file.as_raw_fd()],
    )
    .unwrap();
    send(
        &stream,
        &Message::CursorShape {
            width: 0,
            height: 0,
            hot_x: 0,
            hot_y: 0,
            argb: vec![],
        },
        &[],
    )
    .unwrap();
    send(
        &stream,
        &Message::CursorState {
            x: 16,
            y: 16,
            visible: false,
        },
        &[],
    )
    .unwrap();
    send(
        &stream,
        &Message::FrameReady {
            flags: FrameFlags::default(),
            surface_id: 1,
            buffer: 0,
            seq: 1,
            timestamp_ns: 0,
            damage: vec![Rect {
                x: 0,
                y: 0,
                width: 64,
                height: 32,
            }],
        },
        &[],
    )
    .unwrap();
    let writer = stream.try_clone().unwrap();
    let (sender, receiver) = mpsc::channel();
    let reader = std::thread::spawn(move || {
        while let Ok(packet) = receive(&stream) {
            if sender.send(packet.message).is_err() {
                break;
            }
        }
    });
    // If no present-capable ICD exists, skip with the executable's exact diagnostic.
    let released = receiver.recv_timeout(Duration::from_secs(10));
    if released.is_err() {
        let log = std::fs::read_to_string(&stderr).unwrap();
        if log.contains("no Vulkan graphics/present queue")
            || log.contains("ERROR_INCOMPATIBLE_DRIVER")
        {
            eprintln!("SKIP viewer window: {log}");
            drop(viewer);
            reader.join().unwrap();
            return;
        }
        panic!("viewer did not present/release: {released:?}; {log}");
    }
    assert!(matches!(released.unwrap(), Message::Release { seq: 1, .. }));
    let mut window = 0;
    wait(|| {
        window = connection
            .query_tree(root)
            .unwrap()
            .reply()
            .unwrap()
            .children
            .into_iter()
            .find(|child| {
                connection
                    .get_geometry(*child)
                    .unwrap()
                    .reply()
                    .is_ok_and(|g| g.width > 500)
            })
            .unwrap_or(0);
        window != 0
    });
    let pixel = |x, y| {
        connection
            .get_image(ImageFormat::Z_PIXMAP, window, x, y, 1, 1, u32::MAX)
            .unwrap()
            .reply()
            .unwrap()
            .data
    };
    wait(|| pixel(500, 300)[..3] == [0, 0, 255]);
    assert_eq!(&pixel(10, 10)[..3], &[0, 0, 0], "letterbox is black");
    connection
        .set_input_focus(InputFocus::PARENT, window, x11rb::CURRENT_TIME)
        .unwrap()
        .check()
        .unwrap();
    connection
        .warp_pointer(x11rb::NONE, window, 0, 0, 0, 0, 500, 300)
        .unwrap()
        .check()
        .unwrap();
    connection
        .xtest_fake_input(BUTTON_PRESS_EVENT, 1, x11rb::CURRENT_TIME, root, 0, 0, 0)
        .unwrap()
        .check()
        .unwrap();
    connection.flush().unwrap();
    wait(|| {
        receiver
            .try_iter()
            .any(|message| matches!(message, Message::Buttons { mask: 1 }))
    });
    assert!(
        connection
            .xfixes_get_cursor_image()
            .unwrap()
            .reply()
            .unwrap()
            .cursor_image
            .iter()
            .any(|p| p >> 24 != 0),
        "grab must retain the host cursor when the guest has no shape"
    );
    send(
        &writer,
        &Message::CursorShape {
            width: 2,
            height: 2,
            hot_x: 1,
            hot_y: 1,
            argb: vec![255; 16],
        },
        &[],
    )
    .unwrap();
    send(
        &writer,
        &Message::CursorState {
            x: 16,
            y: 16,
            visible: true,
        },
        &[],
    )
    .unwrap();
    wait(|| pixel(260, 390)[..3] == [255, 255, 255]);
    assert_eq!(
        &pixel(236, 364)[..3],
        &[0, 0, 255],
        "hotspot offsets the cursor quad"
    );
    wait(|| {
        connection
            .xfixes_get_cursor_image()
            .unwrap()
            .reply()
            .unwrap()
            .cursor_image
            .iter()
            .all(|p| p >> 24 == 0)
    });
    send(
        &writer,
        &Message::CursorState {
            x: 24,
            y: 20,
            visible: true,
        },
        &[],
    )
    .unwrap();
    wait(|| pixel(380, 440)[..3] == [255, 255, 255]);
    assert_eq!(
        &pixel(260, 390)[..3],
        &[0, 0, 255],
        "move-only update removes old cursor"
    );
    send(
        &writer,
        &Message::CursorState {
            x: 24,
            y: 20,
            visible: false,
        },
        &[],
    )
    .unwrap();
    wait(|| pixel(380, 440)[..3] == [0, 0, 255]);
    send(
        &writer,
        &Message::CursorShape {
            width: 0,
            height: 0,
            hot_x: 0,
            hot_y: 0,
            argb: vec![],
        },
        &[],
    )
    .unwrap();
    wait(|| {
        connection
            .xfixes_get_cursor_image()
            .unwrap()
            .reply()
            .unwrap()
            .cursor_image
            .iter()
            .any(|p| p >> 24 != 0)
    });
    for (key, down) in [(38, true), (38, false), (37, true), (64, true)] {
        connection
            .xtest_fake_input(
                if down {
                    KEY_PRESS_EVENT
                } else {
                    KEY_RELEASE_EVENT
                },
                key,
                x11rb::CURRENT_TIME,
                root,
                0,
                0,
                0,
            )
            .unwrap()
            .check()
            .unwrap();
    }
    connection.flush().unwrap();
    let mut keys = Vec::new();
    wait(|| {
        keys.extend(receiver.try_iter());
        keys.iter()
            .any(|message| matches!(message,Message::Key { qcode,down:false } if qcode == "ctrl"))
    });
    assert!(keys.contains(&Message::Key {
        qcode: "a".into(),
        down: true
    }));
    assert!(keys.contains(&Message::Key {
        qcode: "a".into(),
        down: false
    }));
    assert!(
        !keys
            .iter()
            .any(|message| matches!(message, Message::Key {qcode, ..} if qcode == "alt")),
        "completing chord key is never forwarded"
    );
    let ungrabbed = || {
        let property = connection
            .get_property(false, window, AtomEnum::WM_NAME, AtomEnum::ANY, 0, 256)
            .unwrap()
            .reply()
            .unwrap();
        String::from_utf8_lossy(&property.value).contains("click to grab")
    };
    wait(ungrabbed);
    for (kind, detail) in [
        (KEY_RELEASE_EVENT, 37),
        (KEY_RELEASE_EVENT, 64),
        (BUTTON_RELEASE_EVENT, 1),
        (KEY_PRESS_EVENT, 64),
        (BUTTON_PRESS_EVENT, 1),
        (BUTTON_RELEASE_EVENT, 1),
    ] {
        connection
            .xtest_fake_input(kind, detail, x11rb::CURRENT_TIME, root, 0, 0, 0)
            .unwrap()
            .check()
            .unwrap();
    }
    connection.flush().unwrap();
    wait(|| !ungrabbed());
    send(&writer, &Message::LedState { mask: 7 }, &[]).unwrap();
    connection
        .xtest_fake_input(KEY_PRESS_EVENT, 37, x11rb::CURRENT_TIME, root, 0, 0, 0)
        .unwrap()
        .check()
        .unwrap();
    connection.flush().unwrap();
    wait(ungrabbed);
    for key in [37, 64] {
        connection
            .xtest_fake_input(KEY_RELEASE_EVENT, key, x11rb::CURRENT_TIME, root, 0, 0, 0)
            .unwrap()
            .check()
            .unwrap();
    }
    connection
        .configure_window(window, &ConfigureWindowAux::new().width(800).height(600))
        .unwrap()
        .check()
        .unwrap();
    connection.flush().unwrap();
    wait(|| {
        receiver.try_iter().any(|message| {
            matches!(
                message,
                Message::RequestSize {
                    width: 800,
                    height: 600
                }
            )
        })
    });
    // An advertised but unimportable modifier must release the frame, request
    // CPU pixels, and continue presenting on this same connection.
    send(
        &writer,
        &Message::SurfaceConfigure {
            surface_id: 2,
            width: 64,
            height: 32,
            drm_fourcc: u32::from_le_bytes(*b"XR24"),
            stride: 256,
            modifier: u64::MAX,
            kind: SurfaceKind::Dmabuf,
            buffers: 1,
        },
        &[file.as_raw_fd()],
    )
    .unwrap();
    send(
        &writer,
        &Message::FrameReady {
            flags: FrameFlags::default(),
            surface_id: 2,
            buffer: 0,
            seq: 2,
            timestamp_ns: 0,
            damage: vec![],
        },
        &[],
    )
    .unwrap();
    let mut fallback = Vec::new();
    wait(|| {
        fallback.extend(receiver.try_iter());
        fallback.contains(&Message::CpuCopy { surface_id: 2 })
    });
    assert!(fallback.contains(&Message::Release {
        surface_id: 2,
        buffer: 0,
        seq: 2
    }));
    let green: Vec<_> = [0, 255, 0, 255]
        .into_iter()
        .cycle()
        .take(64 * 32 * 4)
        .collect();
    file.write_all_at(&green, 0).unwrap();
    send(
        &writer,
        &Message::SurfaceConfigure {
            surface_id: 3,
            width: 64,
            height: 32,
            drm_fourcc: u32::from_le_bytes(*b"XR24"),
            stride: 256,
            modifier: 0,
            kind: SurfaceKind::Shm,
            buffers: 1,
        },
        &[file.as_raw_fd()],
    )
    .unwrap();
    send(
        &writer,
        &Message::FrameReady {
            flags: FrameFlags::default(),
            surface_id: 3,
            buffer: 0,
            seq: 3,
            timestamp_ns: 0,
            damage: vec![],
        },
        &[],
    )
    .unwrap();
    wait(|| {
        receiver
            .try_iter()
            .any(|m| matches!(m, Message::Release { seq: 3, .. }))
    });
    wait(|| pixel(500, 300)[..3] == [0, 255, 0]);
    file.write_all_at(&[255, 0, 0, 255], 8 * 256 + 20 * 4)
        .unwrap();
    send(
        &writer,
        &Message::FrameReady {
            flags: FrameFlags::default(),
            surface_id: 3,
            buffer: 0,
            seq: 4,
            timestamp_ns: 0,
            damage: vec![Rect {
                x: 20,
                y: 8,
                width: 1,
                height: 1,
            }],
        },
        &[],
    )
    .unwrap();
    wait(|| {
        receiver
            .try_iter()
            .any(|m| matches!(m, Message::Release { seq: 4, .. }))
    });
    wait(|| {
        let sample = pixel(256, 206);
        sample[0] > 230 && sample[1] < 25 && sample[2] == 0
    });
    assert_eq!(
        &pixel(500, 300)[..3],
        &[0, 255, 0],
        "partial upload preserves the rest of the image"
    );
    connection.unmap_window(window).unwrap().check().unwrap();
    connection.flush().unwrap();
    let magenta: Vec<_> = [255, 0, 255, 255]
        .into_iter()
        .cycle()
        .take(64 * 32 * 4)
        .collect();
    file.write_all_at(&magenta, 0).unwrap();
    send(
        &writer,
        &Message::FrameReady {
            flags: FrameFlags::default(),
            surface_id: 3,
            buffer: 0,
            seq: 5,
            timestamp_ns: 0,
            damage: vec![Rect {
                x: 0,
                y: 0,
                width: 64,
                height: 32,
            }],
        },
        &[],
    )
    .unwrap();
    wait(|| {
        receiver
            .try_iter()
            .any(|m| matches!(m, Message::Release { seq: 5, .. }))
    });
    connection.map_window(window).unwrap().check().unwrap();
    connection.flush().unwrap();
    wait(|| pixel(500, 300)[..3] == [255, 0, 255]);
    let protocols = connection
        .intern_atom(false, b"WM_PROTOCOLS")
        .unwrap()
        .reply()
        .unwrap()
        .atom;
    let delete = connection
        .intern_atom(false, b"WM_DELETE_WINDOW")
        .unwrap()
        .reply()
        .unwrap()
        .atom;
    connection
        .send_event(
            false,
            window,
            EventMask::NO_EVENT,
            ClientMessageEvent::new(32, window, protocols, [delete, 0, 0, 0, 0]),
        )
        .unwrap()
        .check()
        .unwrap();
    connection.flush().unwrap();
    wait(|| viewer.try_wait().unwrap().is_some());
    assert!(
        viewer.wait().unwrap().success(),
        "viewer close failed: {}",
        std::fs::read_to_string(stderr).unwrap()
    );
    reader.join().unwrap();
}
