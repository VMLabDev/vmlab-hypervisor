mod common;
use common::*;
use std::fs::File;
use std::io::Write;
use std::os::fd::{AsRawFd, OwnedFd};
use std::os::unix::fs::FileExt;
use std::sync::Arc;
use vmlab_hv_display::{
    Capabilities as C, Cursor, Message as M, Pixels, Rect, SurfaceKind, receive, send,
};

fn ready(c: &vmlab_hv_display::Client) -> (u64, u32, u64, Vec<Rect>) {
    let M::FrameReady {
        surface_id,
        buffer,
        seq,
        damage,
        ..
    } = c.receive().unwrap().message
    else {
        panic!("expected FrameReady")
    };
    (surface_id, buffer, seq, damage)
}
fn configure(c: &vmlab_hv_display::Client) -> (u64, Vec<OwnedFd>) {
    let p = c.receive().unwrap();
    let M::SurfaceConfigure {
        surface_id,
        kind: SurfaceKind::Shm,
        buffers: 3,
        ..
    } = p.message
    else {
        panic!("expected shm configure")
    };
    assert_eq!(p.fds.len(), 3);
    (surface_id, p.fds)
}
fn pixels(fd: &OwnedFd, n: usize) -> Vec<u8> {
    let file = File::from(fd.try_clone().unwrap());
    let mut b = vec![0; n];
    file.read_exact_at(&mut b, 0).unwrap();
    b
}

#[test]
fn idle_publications_send_no_frames_but_reconfigure_and_cpu_copy_still_work() {
    let (server, source, _) = server();
    let (client, worker) = client(&server, C::FRAMES | C::CPU_COPY);
    let (surface, _) = configure(&client);
    let (_, buffer, seq, _) = ready(&client);
    client.release(surface, buffer, seq).unwrap();
    source.0.lock().unwrap().damage.clear();
    for _ in 0..100 {
        server.publish().unwrap();
    }
    client
        .stream()
        .set_read_timeout(Some(std::time::Duration::from_millis(100)))
        .unwrap();
    let error = client.receive().unwrap_err();
    assert!(matches!(
        error.kind(),
        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
    ));
    client
        .stream()
        .set_read_timeout(Some(std::time::Duration::from_secs(3)))
        .unwrap();
    client
        .send(&M::CpuCopy {
            surface_id: surface,
        })
        .unwrap();
    let (_, buffer, next_seq, damage) = ready(&client);
    assert_eq!(next_seq, seq);
    assert_eq!(damage, vec![source.0.lock().unwrap().full()]);
    client.release(surface, buffer, next_seq).unwrap();
    source.0.lock().unwrap().generation += 1;
    server.publish().unwrap();
    let (next_surface, _) = configure(&client);
    assert_ne!(surface, next_surface);
    ready(&client);
    drop(client);
    worker.join().unwrap().unwrap();
}

#[test]
fn first_frame_captures_guest_changes_without_connected_clients() {
    let (server, source, _) = server();
    for value in [7, 9] {
        // No producer publication or client is needed to refresh a screenshot.
        *source.0.lock().unwrap() = frame(value, 2, 2);
        let (c, h) = client(&server, C::FRAMES);
        let (_, fds) = configure(&c);
        let (_, buffer, _, damage) = ready(&c);
        assert_eq!(damage, vec![frame(value, 2, 2).full()]);
        assert_eq!(pixels(&fds[buffer as usize], 16), vec![value; 16]);
        drop(c);
        h.join().unwrap().unwrap();
    }
}

#[test]
fn shutdown_interrupts_unfinished_handshakes_and_idle_sessions() {
    for rfb in [false, true] {
        let (server, _, _) = server();
        let (a, b) = socket();
        let worker_server = server.clone();
        let worker = std::thread::spawn(move || {
            if rfb {
                worker_server.serve_rfb(a)
            } else {
                worker_server.serve_native(a)
            }
        });
        server.shutdown();
        assert!(worker.join().unwrap().is_err());
        drop(b);
    }
    let (server, _, _) = server();
    let (client, worker) = client(&server, C::FRAMES);
    configure(&client);
    ready(&client);
    server.shutdown();
    worker.join().unwrap().unwrap();
    drop(client);
}

#[test]
fn cpu_copy_captures_current_source_without_producer_publication() {
    let (server, source, _) = server();
    let (c, h) = client(&server, C::FRAMES | C::CPU_COPY);
    let (sid, _) = configure(&c);
    let held = ready(&c);
    *source.0.lock().unwrap() = frame(8, 2, 2);
    c.cpu_copy(sid).unwrap();
    let (_, fds) = configure(&c);
    let (_, buffer, _, damage) = ready(&c);
    assert_eq!(damage, vec![frame(8, 2, 2).full()]);
    assert_eq!(pixels(&fds[buffer as usize], 16), vec![8; 16]);
    c.release(held.0, held.1, held.2).unwrap();
    drop(c);
    h.join().unwrap().unwrap();
}

#[test]
fn damage_resize_reconnect_and_real_memfds() {
    let (server, source, _) = server();
    let (c, h) = client(&server, C::FRAMES);
    let (sid, fds) = configure(&c);
    let (s, b, q, damage) = ready(&c);
    assert_eq!(sid, s);
    assert_eq!(damage, vec![frame(1, 4, 3).full()]);
    assert_eq!(pixels(&fds[b as usize], 48), vec![1; 48]);
    // Linux marks every received descriptor close-on-exec.
    // SAFETY: fcntl queries a live descriptor without changing it.
    let fd_flags = unsafe { libc::fcntl(fds[0].as_raw_fd(), libc::F_GETFD) };
    assert_ne!(fd_flags & libc::FD_CLOEXEC, 0);
    c.release(s, b, q).unwrap();
    *source.0.lock().unwrap() = frame(2, 4, 3);
    server.publish().unwrap();
    let (s, b, q, damage) = ready(&c);
    assert_eq!(damage, frame(2, 4, 3).damage);
    assert_eq!(pixels(&fds[b as usize], 48), vec![2; 48]);
    c.release(s, b, q).unwrap();
    *source.0.lock().unwrap() = frame(3, 2, 2);
    server.publish().unwrap();
    let (new_sid, new_fds) = configure(&c);
    assert_ne!(new_sid, sid);
    let (_, b, _, damage) = ready(&c);
    assert_eq!(damage, vec![frame(3, 2, 2).full()]);
    assert_eq!(pixels(&new_fds[b as usize], 16), vec![3; 16]);
    drop(c);
    h.join().unwrap().unwrap();
    let (c, h) = client(&server, C::FRAMES);
    configure(&c);
    assert_eq!(ready(&c).3, vec![frame(3, 2, 2).full()]);
    drop(c);
    h.join().unwrap().unwrap();
}

#[test]
fn held_buffers_are_immutable_and_slow_clients_skip_independently() {
    let (server, source, _) = server();
    let (slow, sh) = client(&server, C::FRAMES);
    let (_, fds) = configure(&slow);
    let first = ready(&slow);
    let mut held = vec![first.clone()];
    for value in 2..=3 {
        *source.0.lock().unwrap() = frame(value, 4, 3);
        server.publish().unwrap();
        held.push(ready(&slow));
    }
    let (fast, fh) = client(&server, C::FRAMES);
    configure(&fast);
    let f = ready(&fast);
    let mut last_seq = f.2;
    fast.release(f.0, f.1, f.2).unwrap();
    for value in 4..=30 {
        *source.0.lock().unwrap() = frame(value, 4, 3);
        server.publish().unwrap();
        let f = ready(&fast);
        assert_eq!(f.2, last_seq + 1);
        last_seq = f.2;
        fast.release(f.0, f.1, f.2).unwrap();
    }
    for (i, (_, buffer, _, _)) in held.iter().enumerate() {
        assert_eq!(pixels(&fds[*buffer as usize], 48), vec![(i + 1) as u8; 48]);
    }
    slow.release(first.0, first.1, first.2).unwrap();
    let latest = ready(&slow);
    assert_eq!(latest.2, last_seq);
    assert_eq!(latest.3, vec![frame(30, 4, 3).full()]);
    assert_eq!(pixels(&fds[latest.1 as usize], 48), vec![30; 48]);
    drop(slow);
    drop(fast);
    sh.join().unwrap().unwrap();
    fh.join().unwrap().unwrap();
}

#[test]
fn frame_allocations_plateau_without_clients_and_with_unreleased_buffers() {
    let (server, source, _) = server();
    let mut allocations = Vec::new();
    let mut publish = |value| {
        let next = frame(value, 256, 256);
        let Pixels::Shm(pixels) = &next.pixels else {
            unreachable!()
        };
        allocations.push(Arc::downgrade(pixels));
        *source.0.lock().unwrap() = next;
        server.publish().unwrap();
        let live = allocations
            .iter()
            .filter(|frame| frame.strong_count() != 0)
            .count();
        assert!(live <= 4, "{live} pixel allocations remain live");
    };
    for value in (0..2000).map(|value| value as u8) {
        publish(value);
    }
    let (slow, handle) = client(&server, C::FRAMES);
    configure(&slow);
    ready(&slow);
    for value in [1, 2] {
        publish(value);
        ready(&slow);
    }
    for value in (0..2000).map(|value| value as u8) {
        publish(value);
    }
    drop(slow);
    handle.join().unwrap().unwrap();
    assert_eq!(
        allocations
            .iter()
            .filter(|frame| frame.strong_count() != 0)
            .count(),
        1
    );
}

#[test]
fn preferred_size_requires_input_and_valid_dimensions() {
    for (caps, width, height) in [(0, 800, 600), (C::INPUT, 0, 600), (C::INPUT, 800, 16385)] {
        let (server, _, sink) = server();
        let (c, h) = client(&server, caps);
        if caps == C::INPUT {
            assert!(matches!(c.receive().unwrap().message, M::LedState { .. }));
        }
        c.send(&M::RequestSize { width, height }).unwrap();
        assert!(c.receive().is_err());
        assert!(sink.0.lock().unwrap().is_empty());
        drop(c);
        h.join().unwrap().unwrap();
    }
}

#[test]
fn cursor_leds_and_all_input_messages() {
    let (server, _, sink) = server();
    server
        .set_cursor(Cursor {
            width: 1,
            height: 1,
            hot_x: 0,
            hot_y: 0,
            argb: vec![1, 2, 3, 255],
            x: -1,
            y: 3,
            visible: true,
        })
        .unwrap();
    server.set_leds(5);
    let (c, h) = client(&server, C::CURSOR | C::INPUT);
    assert!(
        matches!(c.receive().unwrap().message,M::CursorShape { argb,.. } if argb == vec![1,2,3,255])
    );
    assert_eq!(
        c.receive().unwrap().message,
        M::CursorState {
            x: -1,
            y: 3,
            visible: true
        }
    );
    assert_eq!(c.receive().unwrap().message, M::LedState { mask: 5 });
    let messages = vec![
        M::Key {
            qcode: "ctrl_r".into(),
            down: true,
        },
        M::PointerAbs {
            x: 2,
            y: 1,
            width: 4,
            height: 3,
        },
        M::Buttons { mask: 1 },
        M::Scroll { dx: -1, dy: 1 },
        M::RequestSize {
            width: 800,
            height: 600,
        },
    ];
    for m in &messages {
        c.send(m).unwrap();
    }
    until(|| (sink.0.lock().unwrap().len() == messages.len()).then_some(()));
    assert_eq!(*sink.0.lock().unwrap(), messages);
    server.set_leds(2);
    assert_eq!(c.receive().unwrap().message, M::LedState { mask: 2 });
    drop(c);
    h.join().unwrap().unwrap();
}

#[test]
fn dmabuf_acquire_fd_and_cpu_copy_reconfigure() {
    let (server, source, _) = server();
    let fd: OwnedFd = File::open("/dev/zero").unwrap().into();
    let sync: OwnedFd = File::open("/dev/null").unwrap().into();
    let mut f = frame(9, 4, 3);
    f.pixels = Pixels::Dmabuf {
        fd: Arc::new(fd),
        acquire: Some(Arc::new(sync)),
    };
    f.modifier = 123;
    *source.0.lock().unwrap() = f;
    server.publish().unwrap();
    let (c, h) = client(&server, C::FRAMES | C::DMABUF_OK | C::CPU_COPY);
    let p = c.receive().unwrap();
    assert!(matches!(
        p.message,
        M::SurfaceConfigure {
            kind: SurfaceKind::Dmabuf,
            buffers: 1,
            modifier: 123,
            ..
        }
    ));
    assert_eq!(p.fds.len(), 1);
    let p = c.receive().unwrap();
    let M::FrameReady {
        surface_id,
        buffer,
        seq,
        ..
    } = p.message
    else {
        panic!()
    };
    assert_eq!(p.fds.len(), 1);
    c.cpu_copy(surface_id).unwrap();
    let (shm, fds) = configure(&c);
    let f = ready(&c);
    assert_ne!(shm, surface_id);
    assert_eq!(pixels(&fds[f.1 as usize], 48), vec![42; 48]);
    c.release(surface_id, buffer, seq).unwrap();
    c.release(f.0, f.1, f.2).unwrap();
    // A later dma-buf frame stays in CPU mode for this connection.
    server.publish().unwrap();
    let f = ready(&c);
    assert_eq!(f.0, shm);
    drop(c);
    h.join().unwrap().unwrap();
    let (c, h) = client(&server, C::FRAMES);
    let (_, fds) = configure(&c);
    let f = ready(&c);
    assert_eq!(pixels(&fds[f.1 as usize], 48), vec![42; 48]);
    drop(c);
    h.join().unwrap().unwrap();
}

#[test]
fn fragmented_streams_and_fd_validation() {
    let (a, mut b) = socket();
    let bytes = M::Hello {
        version: 1,
        capabilities: C(1),
    }
    .encode()
    .unwrap();
    let h = std::thread::spawn(move || {
        for byte in bytes {
            b.write_all(&[byte]).unwrap();
        }
    });
    assert_eq!(
        receive(&a).unwrap().message,
        M::Hello {
            version: 1,
            capabilities: C(1)
        }
    );
    h.join().unwrap();
    let fd = File::open("/dev/null").unwrap();
    assert!(send(&a, &M::LedState { mask: 1 }, &[fd.as_raw_fd()]).is_err());
    let (a, mut b) = socket();
    let mut bytes = M::Hello {
        version: 1,
        capabilities: C(1),
    }
    .encode()
    .unwrap();
    bytes[..4].copy_from_slice(&u32::MAX.to_le_bytes());
    b.write_all(&bytes).unwrap();
    assert!(receive(&a).is_err());
}

#[test]
fn wrong_version_and_unnegotiated_input_disconnect() {
    let (server, _, sink) = server();
    let (a, b) = socket();
    let s = server.clone();
    let h = std::thread::spawn(move || s.serve_native(a));
    send(
        &b,
        &M::Hello {
            version: 2,
            capabilities: C::ALL,
        },
        &[],
    )
    .unwrap();
    assert!(receive(&b).is_err());
    assert!(h.join().unwrap().is_err());
    let (c, h) = client(&server, 0);
    c.send(&M::Key {
        qcode: "a".into(),
        down: true,
    })
    .unwrap();
    assert!(c.receive().is_err());
    h.join().unwrap().unwrap();
    assert!(sink.0.lock().unwrap().is_empty());
}

#[test]
fn same_size_reset_reconfigures_without_overwriting_old_held_buffers() {
    let (server, source, _) = server();
    let (c, h) = client(&server, C::FRAMES);
    let (old_id, old_fds) = configure(&c);
    let old = ready(&c);
    let mut reset = frame(9, 4, 3);
    reset.generation = 1;
    *source.0.lock().unwrap() = reset;
    server.publish().unwrap();
    let (new_id, fds) = configure(&c);
    let new = ready(&c);
    assert_ne!(old_id, new_id);
    assert_eq!(new.3, vec![frame(9, 4, 3).full()]);
    assert_eq!(pixels(&old_fds[old.1 as usize], 48), vec![1; 48]);
    assert_eq!(pixels(&fds[new.1 as usize], 48), vec![9; 48]);
    let file = File::from(fds[0].try_clone().unwrap());
    assert!(file.set_len(1).is_err());
    c.release(old.0, old.1, old.2).unwrap();
    c.release(new.0, new.1, new.2).unwrap();
    drop(c);
    h.join().unwrap().unwrap();
}

#[test]
fn client_connects_to_a_named_socket() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("display.sock");
    let listener = std::os::unix::net::UnixListener::bind(&path).unwrap();
    let (server, _, _) = server();
    let h = std::thread::spawn(move || server.serve_native(listener.accept().unwrap().0));
    let c = vmlab_hv_display::Client::connect(&path, C(C::FRAMES)).unwrap();
    configure(&c);
    assert_eq!(ready(&c).3, vec![frame(1, 4, 3).full()]);
    drop(c);
    h.join().unwrap().unwrap();
    std::fs::remove_file(path).unwrap();
}
