use std::fs::File;
use std::io::{BufRead, BufReader, Write};
use std::os::fd::{AsRawFd, OwnedFd};
use std::os::unix::{
    fs::FileExt,
    net::{UnixListener, UnixStream},
};
use std::sync::Arc;
use std::time::{Duration, Instant};
use vmlab_hv_display::{
    Capabilities, Client, FrameFlags, Message, Rect, SurfaceKind, receive, send,
};
use vmlab_hv_viewer::{
    Core, Options,
    clipboard::{Bridge, HostClipboard, LabClipboard},
    input::{Input, KEYS, pointer, qcode},
    render::pack_damage,
    shm_only,
};
use winit::platform::scancode::PhysicalKeyExtScancode;

fn pair(shm: bool) -> (Core, UnixStream) {
    let (client, server) = UnixStream::pair().unwrap();
    let handshake = server.try_clone().unwrap();
    let thread = std::thread::spawn(move || {
        let hello = receive(&handshake).unwrap();
        let Message::Hello {
            version,
            capabilities,
        } = hello.message
        else {
            panic!("not Hello")
        };
        assert_eq!(capabilities.has(Capabilities::DMABUF_OK), !shm);
        assert!(capabilities.has(Capabilities::STABLE_BUFFERS));
        send(
            &handshake,
            &Message::Hello {
                version,
                capabilities,
            },
            &[],
        )
        .unwrap();
    });
    let mut caps = Capabilities::ALL.0;
    if shm {
        caps &= !Capabilities::DMABUF_OK;
    }
    let core = Core::new(
        Arc::new(Client::from_stream(client, Capabilities(caps)).unwrap()),
        "ctrl+alt",
    )
    .unwrap();
    thread.join().unwrap();
    server
        .set_read_timeout(Some(Duration::from_secs(2)))
        .unwrap();
    (core, server)
}

fn configure(server: &UnixStream, id: u64, files: &[File], kind: SurfaceKind) {
    let fds: Vec<_> = files.iter().map(|f| f.as_raw_fd()).collect();
    send(
        server,
        &Message::SurfaceConfigure {
            surface_id: id,
            width: 4,
            height: 3,
            drm_fourcc: u32::from_le_bytes(*b"XR24"),
            stride: 20,
            modifier: 0,
            kind,
            buffers: files.len() as u32,
        },
        &fds,
    )
    .unwrap();
}
fn frame(server: &UnixStream, id: u64, buffer: u32, seq: u64, damage: Vec<Rect>) {
    send(
        server,
        &Message::FrameReady {
            flags: FrameFlags(FrameFlags::PLACEHOLDER),
            surface_id: id,
            buffer,
            seq,
            timestamp_ns: seq,
            damage,
        },
        &[],
    )
    .unwrap();
}
fn process(core: &mut Core) -> Option<vmlab_hv_viewer::Frame> {
    core.receive(core.client.receive().unwrap()).unwrap()
}
fn buffer(value: u8) -> File {
    let file = tempfile::tempfile().unwrap();
    file.write_all_at(&[value; 60], 0).unwrap();
    file
}

#[test]
fn server_frames_keep_buffers_and_upload_only_damage_across_rotating_slots() {
    let (mut core, server) = pair(true);
    let files = [buffer(7), buffer(99), buffer(55)];
    configure(&server, 1, &files, SurfaceKind::Shm);
    assert!(process(&mut core).is_none());
    let handles: Vec<_> = core.surfaces[&1]
        .buffers
        .iter()
        .map(AsRawFd::as_raw_fd)
        .collect();
    frame(&server, 1, 0, 1, vec![]);
    let first = process(&mut core).unwrap();
    assert!(first.flags.has(FrameFlags::PLACEHOLDER));
    assert_eq!(
        first.damage,
        vec![Rect {
            x: 0,
            y: 0,
            width: 4,
            height: 3
        }]
    );
    core.release(&first).unwrap();
    assert_eq!(
        receive(&server).unwrap().message,
        Message::Release {
            surface_id: 1,
            buffer: 0,
            seq: 1
        }
    );
    let damage = Rect {
        x: 1,
        y: 1,
        width: 2,
        height: 1,
    };
    frame(&server, 1, 1, 2, vec![damage]);
    let second = process(&mut core).unwrap();
    assert_eq!(
        core.surfaces[&1]
            .buffers
            .iter()
            .map(AsRawFd::as_raw_fd)
            .collect::<Vec<_>>(),
        handles
    );
    let pixels = &core.surfaces[&1].pixels;
    assert_eq!(&pixels[24..32], &[99; 8]);
    assert_eq!(&pixels[0..16], &[7; 16]);
    assert_eq!(&pixels[40..56], &[7; 16]);
    let (upload, copies) = pack_damage(pixels, 20, &second.damage);
    assert_eq!(upload, [99; 8]);
    assert_eq!(copies.len(), 1);
    assert_eq!(copies[0].image_offset.x, 1);
    assert_eq!(copies[0].image_offset.y, 1);
    core.release(&second).unwrap();
    receive(&server).unwrap();
    frame(&server, 1, 2, 3, vec![]);
    let third = process(&mut core).unwrap();
    assert!(third.damage.is_empty());
    assert!(
        pack_damage(&core.surfaces[&1].pixels, 20, &third.damage)
            .0
            .is_empty()
    );
    core.release(&third).unwrap();
    receive(&server).unwrap();
    configure(&server, 2, &files[..1], SurfaceKind::Shm);
    process(&mut core);
    assert_eq!(core.surfaces.len(), 1);
    assert!(!core.surfaces.contains_key(&1));
}

#[test]
fn dmabuf_import_identity_is_surface_and_slot_not_sequence() {
    let (mut core, server) = pair(false);
    configure(&server, 99, &[buffer(7)], SurfaceKind::Dmabuf);
    process(&mut core);
    let fd = core.surfaces[&99].buffers[0].as_raw_fd();
    for seq in 1..5 {
        frame(&server, 99, 0, seq, vec![]);
        let next = process(&mut core).unwrap();
        assert_eq!(core.surfaces[&99].buffers[0].as_raw_fd(), fd);
        assert!(core.surfaces[&99].pixels.is_empty());
        core.release(&next).unwrap();
        assert_eq!(
            receive(&server).unwrap().message,
            Message::Release {
                surface_id: 99,
                buffer: 0,
                seq
            }
        );
    }
}

#[test]
fn stable_buffer_formats_and_configure_replacement_follow_extension_contract() {
    let (mut core, server) = pair(false);
    for (id, fourcc) in [*b"XR24", *b"AR24", *b"XB24", *b"AB24"]
        .into_iter()
        .enumerate()
    {
        let message = Message::SurfaceConfigure {
            surface_id: id as u64,
            width: 4,
            height: 3,
            drm_fourcc: u32::from_le_bytes(fourcc),
            stride: 20,
            modifier: 0,
            kind: SurfaceKind::Dmabuf,
            buffers: 1,
        };
        let file = buffer(7);
        send(&server, &message, &[file.as_raw_fd()]).unwrap();
        process(&mut core);
        assert_eq!(core.surfaces.len(), 1);
        assert_eq!(
            core.surfaces[&(id as u64)].fourcc,
            u32::from_le_bytes(fourcc)
        );
    }
    configure(&server, 3, &[buffer(7)], SurfaceKind::Dmabuf);
    assert!(
        core.receive(core.client.receive().unwrap()).is_err(),
        "configure cannot silently reuse a cached import id"
    );
}

#[test]
fn malformed_surfaces_frames_and_damage_are_bounded() {
    let (mut core, server) = pair(true);
    let file = buffer(7);
    configure(&server, 1, &[file], SurfaceKind::Shm);
    process(&mut core);
    frame(&server, 1, 8, 1, vec![]);
    assert!(core.receive(core.client.receive().unwrap()).is_err());
    frame(&server, 9, 0, 1, vec![]);
    assert!(core.receive(core.client.receive().unwrap()).is_err());
    assert_eq!(
        vmlab_hv_viewer::clip(
            Rect {
                x: 3,
                y: 2,
                width: u32::MAX,
                height: u32::MAX
            },
            4,
            3
        ),
        Some(Rect {
            x: 3,
            y: 2,
            width: 1,
            height: 1
        })
    );
    let fds: Vec<OwnedFd> = vec![tempfile::tempfile().unwrap().into()];
    assert!(
        core.receive(vmlab_hv_display::Packet {
            message: Message::SurfaceConfigure {
                surface_id: 2,
                width: 16385,
                height: 1,
                drm_fourcc: 0,
                stride: 4,
                modifier: 0,
                kind: SurfaceKind::Shm,
                buffers: 1
            },
            fds
        })
        .is_err()
    );
}

#[test]
fn keyboard_pointer_resize_and_grab_cross_the_fake_socket() {
    let (mut core, server) = pair(true);
    core.input.grab();
    for &(code, name) in KEYS {
        assert_eq!(
            winit::keyboard::PhysicalKey::from_scancode(code).to_scancode(),
            Some(code)
        );
        let down = core.input.key(code, true);
        core.send(down).unwrap();
        assert_eq!(
            receive(&server).unwrap().message,
            Message::Key {
                qcode: name.into(),
                down: true
            }
        );
        let up = core.input.key(code, false);
        core.send(up).unwrap();
        assert_eq!(
            receive(&server).unwrap().message,
            Message::Key {
                qcode: name.into(),
                down: false
            }
        );
    }
    assert_eq!(
        KEYS.iter()
            .map(|(code, _)| code)
            .collect::<std::collections::BTreeSet<_>>()
            .len(),
        KEYS.len()
    );
    for code in 1..=83 {
        assert!(qcode(code).is_some(), "missing main keyboard code {code}");
    }
    for (code, name) in [
        (30, "a"),
        (97, "ctrl_r"),
        (100, "alt_r"),
        (86, "less"),
        (96, "kp_enter"),
        (124, "yen"),
        (183, "f13"),
        (194, "f24"),
        (113, "audiomute"),
    ] {
        assert_eq!(qcode(code), Some(name));
    }
    assert_eq!(qcode(0xffff), None);
    let pointer = pointer((500., 300.), (1000, 600), (800, 600)).unwrap();
    assert_eq!(
        pointer,
        Message::PointerAbs {
            x: 400,
            y: 300,
            width: 800,
            height: 600
        }
    );
    core.send([pointer]).unwrap();
    assert_eq!(
        receive(&server).unwrap().message,
        Message::PointerAbs {
            x: 400,
            y: 300,
            width: 800,
            height: 600
        }
    );
    let now = Instant::now();
    core.resize(now, 900, 700);
    core.resize(now + Duration::from_millis(190), 1200, 800);
    core.tick(now + Duration::from_millis(389)).unwrap();
    core.tick(now + Duration::from_millis(390)).unwrap();
    assert_eq!(
        receive(&server).unwrap().message,
        Message::RequestSize {
            width: 1200,
            height: 800
        }
    );
    core.tick(now + Duration::from_secs(1)).unwrap();
    let down = core.input.key(29, true);
    core.send(down).unwrap();
    assert_eq!(
        receive(&server).unwrap().message,
        Message::Key {
            qcode: "ctrl".into(),
            down: true
        }
    );
    let release = core.input.key(56, true);
    assert!(!core.input.grabbed);
    assert!(release.contains(&Message::Key {
        qcode: "ctrl".into(),
        down: false
    }));
    assert!(core.input.key(30, true).is_empty());
}

#[test]
fn configurable_chord_focus_loss_buttons_repeat_and_pointer_edges() {
    let mut input = Input::new("shift_r+f12").unwrap();
    input.grab();
    assert!(!input.key(30, true).is_empty());
    assert!(input.key(30, true).is_empty());
    input.button(1, true);
    let released = input.release();
    assert!(released.contains(&Message::Key {
        qcode: "a".into(),
        down: false
    }));
    assert!(released.contains(&Message::Buttons { mask: 0 }));
    input.grab();
    input.key(54, true);
    input.key(88, true);
    assert!(!input.grabbed);
    assert!(Input::new("ctrl+ctrl").is_err());
    assert!(Input::new("garbage").is_err());
    assert_eq!(
        pointer((-50., -50.), (1000, 600), (800, 600)),
        Some(Message::PointerAbs {
            x: 0,
            y: 0,
            width: 800,
            height: 600
        })
    );
    assert_eq!(
        pointer((2000., 2000.), (1000, 600), (800, 600)),
        Some(Message::PointerAbs {
            x: 799,
            y: 599,
            width: 800,
            height: 600
        })
    );
    assert!(pointer((0., 0.), (0, 600), (800, 600)).is_none());
    assert!(pointer((f64::NAN, 0.), (800, 600), (800, 600)).is_none());
}

#[test]
fn release_chord_tracks_keys_held_before_grab_in_either_order() {
    for (first, second) in [(29, 56), (56, 29)] {
        let (mut core, server) = pair(true);
        assert!(core.input.key(first, true).is_empty());
        core.input.grab();
        for mask in [7, 0, 2] {
            send(&server, &Message::LedState { mask }, &[]).unwrap();
            process(&mut core);
        }
        let messages = core.input.key(second, true);
        assert!(!core.input.grabbed, "first chord press must release");
        assert!(messages.is_empty(), "neither chord key reached the guest");
        core.input.key(second, false);
        core.input.grab();
        core.input.key(second, true);
        assert!(!core.input.grabbed, "a held modifier survives re-grab");
    }
}

#[test]
fn release_chord_sends_breaks_only_for_forwarded_keys() {
    for (first, second, name) in [(29, 56, "ctrl"), (56, 29, "alt")] {
        let (mut core, server) = pair(true);
        core.input.grab();
        let messages = core.input.key(first, true);
        core.send(messages).unwrap();
        assert_eq!(
            receive(&server).unwrap().message,
            Message::Key {
                qcode: name.into(),
                down: true,
            }
        );
        let messages = core.input.key(second, true);
        assert!(!core.input.grabbed);
        assert_eq!(
            messages,
            vec![Message::Key {
                qcode: name.into(),
                down: false,
            }]
        );
        core.send(messages).unwrap();
        assert_eq!(
            receive(&server).unwrap().message,
            Message::Key {
                qcode: name.into(),
                down: false,
            }
        );
    }
}

#[test]
fn focus_loss_discards_physical_state_and_releases_forwarded_input() {
    let mut input = Input::new("ctrl+alt").unwrap();
    input.grab();
    input.key(29, true);
    input.button(1, true);
    assert_eq!(
        input.focus_lost(),
        vec![
            Message::Key {
                qcode: "ctrl".into(),
                down: false
            },
            Message::Buttons { mask: 0 },
        ]
    );
    input.grab();
    assert_eq!(
        input.key(56, true),
        vec![Message::Key {
            qcode: "alt".into(),
            down: true
        }]
    );
    assert!(input.grabbed);
}

#[test]
fn cursor_fallback_shape_hotspot_move_only_and_visibility() {
    let (mut core, server) = pair(true);
    core.input.grab();
    assert!(core.host_cursor_visible());
    let shape = Message::CursorShape {
        width: 2,
        height: 2,
        hot_x: 1,
        hot_y: 1,
        argb: vec![255; 16],
    };
    send(&server, &shape, &[]).unwrap();
    process(&mut core);
    let generation = core.cursor.generation;
    assert!(!core.host_cursor_visible());
    for (x, y, visible) in [(5, 6, true), (-1, 9, true), (7, 8, false)] {
        send(&server, &Message::CursorState { x, y, visible }, &[]).unwrap();
        process(&mut core);
        assert_eq!(
            (core.cursor.x, core.cursor.y, core.cursor.visible),
            (x, y, visible)
        );
        assert_eq!(core.cursor.generation, generation);
        assert_eq!((core.cursor.hot_x, core.cursor.hot_y), (1, 1));
        assert_eq!(core.cursor.argb, vec![255; 16]);
        assert!(
            !core.host_cursor_visible(),
            "explicit guest hide remains authoritative"
        );
    }
    core.input.release();
    assert!(core.host_cursor_visible());
    core.input.grab();
    send(
        &server,
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
    process(&mut core);
    assert!(core.host_cursor_visible());
    let fallback = vmlab_hv_viewer::Cursor::fallback();
    assert!(fallback.has_shape() && fallback.visible);
    assert_eq!(
        fallback.argb.len(),
        (fallback.width * fallback.height * 4) as usize
    );
    assert!(fallback.argb.as_chunks::<4>().0.iter().any(|p| p[3] != 0));
}

#[test]
fn cursor_led_and_cli_wslg_negotiation() {
    let (mut core, server) = pair(true);
    for message in [
        Message::CursorShape {
            width: 1,
            height: 1,
            hot_x: 0,
            hot_y: 0,
            argb: vec![1, 2, 3, 255],
        },
        Message::CursorState {
            x: 5,
            y: 6,
            visible: true,
        },
        Message::LedState { mask: 7 },
    ] {
        send(&server, &message, &[]).unwrap();
        process(&mut core);
    }
    assert_eq!(core.cursor.argb, [1, 2, 3, 255]);
    assert_eq!(
        (core.cursor.x, core.cursor.y, core.cursor.visible),
        (5, 6, true)
    );
    assert_eq!(core.leds, 7);
    assert!(shm_only(false, true, None));
    assert!(shm_only(
        false,
        false,
        Some("/mnt/wslg/runtime-dir/wayland-0")
    ));
    assert!(shm_only(true, false, None));
    assert!(!shm_only(false, false, Some("wayland-0")));
    let options = Options::parse(
        [
            "--display",
            "/tmp/display",
            "--shm",
            "--release-chord",
            "ctrl_r+alt_r",
        ]
        .map(str::to_owned),
    )
    .unwrap();
    assert!(options.shm);
    assert!(
        Options::parse(["--display", "/tmp/display", "--machine", "test"].map(str::to_owned))
            .is_err()
    );
}

#[derive(Default)]
struct Host {
    text: String,
    writes: usize,
}
impl HostClipboard for Host {
    fn get(&mut self) -> anyhow::Result<String> {
        Ok(self.text.clone())
    }
    fn set(&mut self, text: String) -> anyhow::Result<()> {
        self.text = text;
        self.writes += 1;
        Ok(())
    }
}

#[test]
fn clipboard_focus_host_changes_guest_polling_and_echo_suppression() {
    let directory = tempfile::tempdir().unwrap();
    let socket = directory.path().join("lab.sock");
    let listener = UnixListener::bind(&socket).unwrap();
    let thread = std::thread::spawn(move || {
        for (command, text) in [
            ("machine.clipboard_set", "host"),
            ("machine.clipboard_get", "host"),
            ("machine.clipboard_get", "guest"),
            ("machine.clipboard_set", "changed"),
            ("machine.clipboard_get", "changed"),
            ("machine.clipboard_set", "changed"),
            ("machine.clipboard_get", "changed"),
        ] {
            let (mut stream, _) = listener.accept().unwrap();
            let mut line = String::new();
            BufReader::new(stream.try_clone().unwrap())
                .read_line(&mut line)
                .unwrap();
            let request: serde_json::Value = serde_json::from_str(&line).unwrap();
            assert_eq!(request["type"], "req");
            assert_eq!(request["cmd"], command);
            assert_eq!(request["args"]["machine"], "vm");
            if command.ends_with("set") {
                assert_eq!(request["args"]["text"], text);
            }
            writeln!(
                stream,
                "{}",
                serde_json::json!({"type":"event","event":"unrelated","data":{}})
            )
            .unwrap();
            writeln!(stream,"{}",serde_json::json!({"type":"resp","id":request["id"],"ok":if command.ends_with("get") { serde_json::json!(text) } else { serde_json::Value::Null }})).unwrap();
        }
    });
    let mut host = Host {
        text: "host".into(),
        writes: 0,
    };
    let mut lab = LabClipboard::new(socket, "vm".into());
    let mut bridge = Bridge::default();
    let now = Instant::now();
    bridge.tick(now, &mut host, &mut lab).unwrap();
    bridge.focus(true);
    bridge.tick(now, &mut host, &mut lab).unwrap();
    bridge
        .tick(now + Duration::from_millis(999), &mut host, &mut lab)
        .unwrap();
    bridge
        .tick(now + Duration::from_secs(1), &mut host, &mut lab)
        .unwrap();
    assert_eq!(host.text, "guest");
    assert_eq!(host.writes, 1);
    bridge
        .tick(now + Duration::from_millis(1100), &mut host, &mut lab)
        .unwrap();
    host.text = "changed".into();
    bridge
        .tick(now + Duration::from_millis(1200), &mut host, &mut lab)
        .unwrap();
    bridge
        .tick(now + Duration::from_secs(2), &mut host, &mut lab)
        .unwrap();
    bridge.focus(false);
    bridge
        .tick(now + Duration::from_secs(4), &mut host, &mut lab)
        .unwrap();
    bridge.focus(true);
    bridge
        .tick(now + Duration::from_secs(5), &mut host, &mut lab)
        .unwrap();
    thread.join().unwrap();
}
