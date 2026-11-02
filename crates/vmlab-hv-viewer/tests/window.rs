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
        xproto::{
            BUTTON_PRESS_EVENT, ConfigureWindowAux, ConnectionExt, ImageFormat, InputFocus,
            KEY_PRESS_EVENT, KEY_RELEASE_EVENT,
        },
        xtest::ConnectionExt as _,
    },
};

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
    if !Command::new("Xvfb").arg("-help").output().is_ok() {
        eprintln!("SKIP viewer window: Xvfb unavailable");
        return;
    }
    // SAFETY: the loader is kept alive through this capability probe.
    let vulkan = unsafe { ash::Entry::load() };
    if vulkan.is_err() {
        eprintln!("SKIP viewer window: Vulkan loader unavailable");
        return;
    }
    let directory = tempfile::tempdir().unwrap();
    let mut xvfb = ManagedChild::helper(
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
    .unwrap();
    let mut number = String::new();
    BufReader::new(xvfb.stdout.take().unwrap())
        .read_line(&mut number)
        .unwrap();
    assert!(
        number.trim().parse::<u16>().is_ok(),
        "Xvfb did not allocate a private display"
    );
    let display = format!(":{}", number.trim());
    let (connection, screen) = x11rb::connect(Some(&display)).unwrap();
    let root = connection.setup().roots[screen].root;
    let socket = directory.path().join("display.sock");
    let listener = UnixListener::bind(&socket).unwrap();
    listener.set_nonblocking(true).unwrap();
    let stderr = directory.path().join("viewer.log");
    let mut viewer = ManagedChild::helper(
        Command::new(env!("CARGO_BIN_EXE_vmlab-hv-viewer"))
            .args(["--display"])
            .arg(&socket)
            .arg("--shm")
            .env("DISPLAY", &display)
            .env_remove("WAYLAND_DISPLAY")
            .env("WINIT_UNIX_BACKEND", "x11")
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
            width: 1,
            height: 1,
            hot_x: 0,
            hot_y: 0,
            argb: vec![255; 4],
        },
        &[],
    )
    .unwrap();
    send(
        &stream,
        &Message::CursorState {
            x: 16,
            y: 16,
            visible: true,
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
    assert_eq!(
        &pixel(260, 390)[..3],
        &[255, 255, 255],
        "protocol cursor is composited"
    );
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
    drop(viewer);
    reader.join().unwrap();
}
