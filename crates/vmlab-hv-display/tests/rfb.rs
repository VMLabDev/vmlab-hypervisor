mod common;
use common::*;
use std::io::{Read, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::sync::atomic::{AtomicU64, Ordering};
use vmlab_hv_display::rfb::*;
use vmlab_hv_display::{Cursor, Message as M};

fn read<const N: usize>(s: &mut UnixStream) -> [u8; N] {
    let mut b = [0; N];
    s.read_exact(&mut b).unwrap();
    b
}
fn handshake(s: &mut UnixStream) {
    assert_eq!(&read::<12>(s), b"RFB 003.008\n");
    s.write_all(b"RFB 003.008\n").unwrap();
    assert_eq!(read::<2>(s), [1, 1]);
    s.write_all(&[1]).unwrap();
    assert_eq!(read::<4>(s), [0; 4]);
    s.write_all(&[1]).unwrap();
    assert_eq!(read::<4>(s), [0, 4, 0, 3]);
    assert_eq!(read::<16>(s), PIXEL_FORMAT);
    let n = u32::from_be_bytes(read::<4>(s));
    let mut name = vec![0; n as usize];
    s.read_exact(&mut name).unwrap();
    assert_eq!(name, b"vmlab-hv");
}

#[test]
fn placeholder_flag_keeps_rfb_pixels_and_active_black_frames() {
    let (server, source, _) = server();
    source.0.lock().unwrap().flags =
        vmlab_hv_display::FrameFlags(vmlab_hv_display::FrameFlags::PLACEHOLDER);
    server.publish().unwrap();
    let (a, mut b) = socket();
    let publisher = server.clone();
    let worker = std::thread::spawn(move || publisher.serve_rfb(a));
    handshake(&mut b);
    request(&mut b, false);
    assert_eq!(update(&mut b)[0].p, [1, 1, 1, 0].repeat(12));
    let mut black = frame(0, 4, 3);
    black.damage.clear();
    *source.0.lock().unwrap() = black;
    server.publish().unwrap();
    request(&mut b, true);
    assert_eq!(update(&mut b)[0].p, vec![0; 4 * 3 * 4]);
    drop(b);
    assert!(worker.join().unwrap().is_ok());
}
fn encodings(s: &mut UnixStream, e: &[i32]) {
    let mut b = vec![2, 0];
    b.extend((e.len() as u16).to_be_bytes());
    for e in e {
        b.extend(e.to_be_bytes());
    }
    s.write_all(&b).unwrap();
}
fn request(s: &mut UnixStream, incremental: bool) {
    s.write_all(&[3, u8::from(incremental), 0, 0, 0, 0, 0, 4, 0, 3])
        .unwrap();
}
#[derive(Debug)]
struct R {
    x: u16,
    y: u16,
    w: u16,
    h: u16,
    e: i32,
    p: Vec<u8>,
}
fn update(s: &mut UnixStream) -> Vec<R> {
    let b = read::<4>(s);
    assert_eq!(&b[..2], &[0, 0]);
    (0..u16::from_be_bytes([b[2], b[3]]))
        .map(|_| {
            let b = read::<12>(s);
            let x = u16::from_be_bytes([b[0], b[1]]);
            let y = u16::from_be_bytes([b[2], b[3]]);
            let w = u16::from_be_bytes([b[4], b[5]]);
            let h = u16::from_be_bytes([b[6], b[7]]);
            let e = i32::from_be_bytes(b[8..12].try_into().unwrap());
            let n = match e {
                RAW => w as usize * h as usize * 4,
                DESKTOP_SIZE => 0,
                ALPHA_CURSOR => {
                    assert_eq!(read::<4>(s), [0; 4]);
                    w as usize * h as usize * 4
                }
                RICH_CURSOR => w as usize * h as usize * 4 + (w as usize).div_ceil(8) * h as usize,
                LED_STATE => 1,
                _ => panic!("unexpected encoding {e}"),
            };
            let mut p = vec![0; n];
            s.read_exact(&mut p).unwrap();
            R { x, y, w, h, e, p }
        })
        .collect()
}

#[test]
fn first_rfb_frame_captures_changes_without_connected_clients() {
    let (server, source, _) = server();
    for value in [7, 9] {
        *source.0.lock().unwrap() = frame(value, 4, 3);
        let (a, mut b) = socket();
        let s = server.clone();
        let h = std::thread::spawn(move || s.serve_rfb(a));
        handshake(&mut b);
        request(&mut b, false);
        assert_eq!(update(&mut b)[0].p, [value, value, value, 0].repeat(12));
        drop(b);
        h.join().unwrap().unwrap();
    }
}

#[test]
fn rfb38_raw_resize_cursors_leds_and_input() {
    let (server, source, sink) = server();
    server
        .set_cursor(Cursor {
            width: 1,
            height: 1,
            argb: vec![2, 3, 4, 128],
            visible: true,
            ..Cursor::default()
        })
        .unwrap();
    server.set_leds(5);
    let (a, mut b) = socket();
    let s = server.clone();
    let h = std::thread::spawn(move || s.serve_rfb(a));
    handshake(&mut b);
    b.write_all(&[0, 0, 0, 0]).unwrap();
    b.write_all(&PIXEL_FORMAT).unwrap();
    encodings(
        &mut b,
        &[RAW, DESKTOP_SIZE, ALPHA_CURSOR, RICH_CURSOR, LED_STATE],
    );
    request(&mut b, false);
    let u = update(&mut b);
    assert_eq!(u.len(), 3);
    assert_eq!(u[0].e, RAW);
    assert_eq!((u[0].x, u[0].y), (0, 0));
    assert_eq!(u[0].p, [1, 1, 1, 0].repeat(12));
    assert_eq!(u[1].e, ALPHA_CURSOR);
    assert_eq!(u[1].p, vec![2, 3, 4, 128]);
    assert_eq!(u[2].p, vec![5]);
    // A pending incremental request must not prevent input delivery.
    request(&mut b, true);
    b.write_all(&[4, 1, 0, 0, 0, 0, 0xff, 0xe3]).unwrap();
    b.write_all(&[5, 9, 0, 2, 0, 1]).unwrap();
    until(|| (sink.0.lock().unwrap().len() == 4).then_some(()));
    assert_eq!(
        *sink.0.lock().unwrap(),
        vec![
            M::Key {
                qcode: "ctrl".into(),
                down: true
            },
            M::PointerAbs {
                x: 2,
                y: 1,
                width: 4,
                height: 3
            },
            M::Buttons { mask: 1 },
            M::Scroll { dx: 0, dy: -1 }
        ]
    );
    *source.0.lock().unwrap() = frame(7, 4, 3);
    server.publish().unwrap();
    assert_eq!(update(&mut b)[0].p, [7, 7, 7, 0].repeat(12));
    encodings(&mut b, &[RAW, DESKTOP_SIZE, RICH_CURSOR, LED_STATE]);
    request(&mut b, false);
    let u = update(&mut b);
    let cursor = u.iter().find(|r| r.e == RICH_CURSOR).unwrap();
    assert_eq!(cursor.p, vec![2, 3, 4, 0, 128]);
    *source.0.lock().unwrap() = frame(8, 2, 2);
    server.publish().unwrap();
    request(&mut b, true);
    let u = update(&mut b);
    assert_eq!((u[0].e, u[0].w, u[0].h), (DESKTOP_SIZE, 2, 2));
    request(&mut b, false);
    assert_eq!(update(&mut b)[0].p, [8, 8, 8, 0].repeat(4));
    server.set_cursor(Cursor::default()).unwrap();
    request(&mut b, true);
    let u = update(&mut b);
    let cursor = u.iter().find(|r| r.e == RICH_CURSOR).unwrap();
    assert_eq!((cursor.w, cursor.h), (0, 0));
    drop(b);
    assert!(h.join().unwrap().is_ok());
}

#[test]
fn raw_respects_requested_region_and_rgb565_big_endian() {
    let (server, source, _) = server();
    let mut f = frame(0, 4, 3);
    f.pixels = vmlab_hv_display::Pixels::Shm(std::sync::Arc::new([0, 0, 255, 0].repeat(12)));
    *source.0.lock().unwrap() = f;
    server.publish().unwrap();
    let (a, mut b) = socket();
    let h = std::thread::spawn(move || server.serve_rfb(a));
    handshake(&mut b);
    b.write_all(&[
        0, 0, 0, 0, 16, 16, 1, 1, 0, 31, 0, 63, 0, 31, 11, 5, 0, 0, 0, 0,
    ])
    .unwrap();
    b.write_all(&[3, 0, 0, 1, 0, 1, 0, 2, 0, 1]).unwrap();
    assert_eq!(read::<4>(&mut b), [0, 0, 0, 1]);
    let r = read::<12>(&mut b);
    assert_eq!(r, [0, 1, 0, 1, 0, 2, 0, 1, 0, 0, 0, 0]);
    assert_eq!(read::<4>(&mut b), [248, 0, 248, 0]);
    drop(b);
    h.join().unwrap().unwrap();
}

#[test]
fn keysym_mapping_covers_vmlab_keys_and_keypad() {
    for (sym, q) in [
        (0x61, "a"),
        (0x41, "a"),
        (0xff0d, "ret"),
        (0xffe7, "meta_l"),
        (0xffc9, "f12"),
        (0xffb3, "kp_3"),
        (0xff9b, "kp_3"),
        (0xffe4, "ctrl_r"),
        (0x21, "1"),
        (0xff61, "print"),
    ] {
        assert_eq!(keysym_to_qcode(sym), Some(q));
    }
    assert_eq!(keysym_to_qcode(0x1001f600), None);
}

static COUNTER: AtomicU64 = AtomicU64::new(0);
#[test]
fn independent_python_rfb_client_on_a_named_unix_socket() {
    let path = std::env::temp_dir().join(format!(
        "vmlab-display-{}-{}.sock",
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::Relaxed)
    ));
    let listener = UnixListener::bind(&path).unwrap();
    let (server, _, _) = server();
    let h = std::thread::spawn(move || {
        let (stream, _) = listener.accept().unwrap();
        server.serve_rfb(stream)
    });
    let result = std::process::Command::new("python3")
        .arg("-c")
        .arg(
            r#"
import socket, struct, sys
s = socket.socket(socket.AF_UNIX)
s.settimeout(3)
s.connect(sys.argv[1])
def read(n):
    b = b''
    while len(b) < n:
        part = s.recv(n-len(b))
        assert part, 'unexpected EOF'
        b += part
    return b
assert read(12) == b'RFB 003.008\n'
s.sendall(b'RFB 003.008\n')
assert read(2) == b'\x01\x01'
s.sendall(b'\x01')
assert read(4) == bytes(4)
s.sendall(b'\x01')
w, h = struct.unpack('>HH', read(4))
assert (w, h) == (4, 3)
assert read(16) == bytes([32,24,0,1,0,255,0,255,0,255,16,8,0,0,0,0])
assert read(struct.unpack('>I', read(4))[0]) == b'vmlab-hv'
s.sendall(struct.pack('>BBHHHH', 3, 0, 0, 0, w, h))
assert read(4) == bytes([0,0,0,1])
assert struct.unpack('>HHHHi', read(12)) == (0,0,w,h,0)
assert read(w*h*4) == bytes([1,1,1,0])*(w*h)
s.close()
print('RFB 3.8 handshake and Raw pixels passed')
"#,
        )
        .arg(&path)
        .output()
        .unwrap();
    std::fs::remove_file(&path).unwrap();
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    h.join().unwrap().unwrap();
}
