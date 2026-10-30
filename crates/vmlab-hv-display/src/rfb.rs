//! RFB 3.8, RFC 6143, with the cursor and LED extensions used by vmlab.

use crate::{DisplayServer, Message, invalid};
use std::io::{self, Read, Write};
use std::net::Shutdown;
use std::os::unix::net::UnixStream;
use std::sync::{Arc, Mutex};
use std::time::Duration;

pub const PIXEL_FORMAT: [u8; 16] = [32, 24, 0, 1, 0, 255, 0, 255, 0, 255, 16, 8, 0, 0, 0, 0];
pub const RAW: i32 = 0;
pub const DESKTOP_SIZE: i32 = -223;
pub const RICH_CURSOR: i32 = -239;
pub const ALPHA_CURSOR: i32 = -314;
pub const LED_STATE: i32 = -261;

fn read<const N: usize>(s: &mut UnixStream) -> io::Result<[u8; N]> {
    let mut b = [0; N];
    s.read_exact(&mut b)?;
    Ok(b)
}
fn be16(b: &[u8]) -> u16 {
    u16::from_be_bytes(b.try_into().unwrap())
}
fn be32(b: &[u8]) -> u32 {
    u32::from_be_bytes(b.try_into().unwrap())
}
fn rect(out: &mut Vec<u8>, x: u16, y: u16, w: u16, h: u16, enc: i32) {
    for v in [x, y, w, h] {
        out.extend(v.to_be_bytes());
    }
    out.extend(enc.to_be_bytes());
}
#[derive(Clone)]
struct Format([u8; 16]);
impl Format {
    fn validate(&self) -> io::Result<()> {
        let b = &self.0;
        if ![8, 16, 32].contains(&b[0]) || b[1] > b[0] || b[2] > 1 || b[3] != 1 {
            return Err(invalid("unsupported RFB pixel format"));
        }
        let mut mask = 0u64;
        for i in 0..3 {
            let max = be16(&b[4 + i * 2..6 + i * 2]) as u64;
            let shift = b[10 + i];
            if max == 0
                || max & (max + 1) != 0
                || shift >= b[0]
                || (max << shift) >> b[0] != 0
                || mask & (max << shift) != 0
            {
                return Err(invalid("invalid RFB colour channels"));
            }
            mask |= max << shift;
        }
        Ok(())
    }
    fn pixel(&self, out: &mut Vec<u8>, bgra: &[u8]) {
        let mut v = 0u32;
        for (i, c) in [bgra[2], bgra[1], bgra[0]].iter().enumerate() {
            let max = be16(&self.0[4 + i * 2..6 + i * 2]) as u32;
            v |= ((*c as u32 * max + 127) / 255) << self.0[10 + i];
        }
        let n = self.0[0] as usize / 8;
        if self.0[2] == 0 {
            out.extend(&v.to_le_bytes()[..n]);
        } else {
            out.extend(&v.to_be_bytes()[4 - n..]);
        }
    }
}
struct ClientState {
    format: Format,
    encodings: Vec<i32>,
    request: Option<[u8; 9]>,
    closed: bool,
    encoding_seq: u64,
}
impl DisplayServer {
    pub fn serve_rfb(&self, mut stream: UnixStream) -> io::Result<()> {
        let _session = self.session(&stream)?;
        stream.write_all(b"RFB 003.008\n")?;
        if &read::<12>(&mut stream)? != b"RFB 003.008\n" {
            return Err(invalid("RFB 3.8 required"));
        }
        stream.write_all(&[1, 1])?;
        if read::<1>(&mut stream)?[0] != 1 {
            return Err(invalid("RFB None security required"));
        }
        stream.write_all(&0u32.to_be_bytes())?;
        read::<1>(&mut stream)?;
        self.publish()?;
        let (frame, ..) = self.snapshot();
        stream.write_all(&(frame.width as u16).to_be_bytes())?;
        stream.write_all(&(frame.height as u16).to_be_bytes())?;
        stream.write_all(&PIXEL_FORMAT)?;
        stream.write_all(&8u32.to_be_bytes())?;
        stream.write_all(b"vmlab-hv")?;
        let state = Arc::new(Mutex::new(ClientState {
            format: Format(PIXEL_FORMAT),
            encodings: vec![RAW],
            request: None,
            closed: false,
            encoding_seq: 0,
        }));
        let local = state.clone();
        let mut reader = stream.try_clone()?;
        let server = self.clone();
        let thread = std::thread::spawn(move || {
            let result = server.rfb_input(&mut reader, &local);
            local.lock().unwrap().closed = true;
            let _ = reader.shutdown(Shutdown::Both);
            result
        });
        let result = self.rfb_updates(&mut stream, &state, (frame.width, frame.height));
        let _ = stream.shutdown(Shutdown::Both);
        let _ = thread.join();
        result
    }
    fn rfb_input(&self, stream: &mut UnixStream, state: &Mutex<ClientState>) -> io::Result<()> {
        let mut buttons = 0u8;
        loop {
            match read::<1>(stream)?[0] {
                0 => {
                    read::<3>(stream)?;
                    let format = Format(read::<16>(stream)?);
                    format.validate()?;
                    state.lock().unwrap().format = format;
                }
                2 => {
                    let header = read::<3>(stream)?;
                    let n = be16(&header[1..]) as usize;
                    if n > 256 {
                        return Err(invalid("too many RFB encodings"));
                    }
                    let mut encodings = Vec::with_capacity(n);
                    for _ in 0..n {
                        encodings.push(i32::from_be_bytes(read::<4>(stream)?));
                    }
                    let mut s = state.lock().unwrap();
                    s.encodings = encodings;
                    s.encoding_seq += 1;
                }
                3 => {
                    state.lock().unwrap().request = Some(read::<9>(stream)?);
                }
                4 => {
                    let b = read::<7>(stream)?;
                    if let Some(qcode) = keysym_to_qcode(be32(&b[3..])) {
                        self.input(Message::Key {
                            qcode: qcode.to_owned(),
                            down: b[0] != 0,
                        });
                    }
                }
                5 => {
                    let b = read::<5>(stream)?;
                    let (frame, ..) = self.snapshot();
                    self.input(Message::PointerAbs {
                        x: (be16(&b[1..3]) as u32).min(frame.width - 1),
                        y: (be16(&b[3..]) as u32).min(frame.height - 1),
                        width: frame.width,
                        height: frame.height,
                    });
                    // Native buttons: left, middle, right. Wheel bits are edge pulses.
                    self.input(Message::Buttons {
                        mask: (b[0] & 7) as u32,
                    });
                    let pressed = b[0] & !buttons;
                    let dx = i32::from(pressed & 64 != 0) - i32::from(pressed & 32 != 0);
                    let dy = i32::from(pressed & 16 != 0) - i32::from(pressed & 8 != 0);
                    if dx != 0 || dy != 0 {
                        self.input(Message::Scroll { dx, dy });
                    }
                    buttons = b[0];
                }
                6 => {
                    let b = read::<7>(stream)?;
                    let n = be32(&b[3..]) as usize;
                    if n > 1024 * 1024 {
                        return Err(invalid("RFB clipboard too large"));
                    }
                    io::copy(&mut stream.take(n as u64), &mut io::sink())?;
                }
                _ => return Err(invalid("unsupported RFB client message")),
            }
        }
    }
    fn rfb_updates(
        &self,
        stream: &mut UnixStream,
        state: &Mutex<ClientState>,
        mut size: (u32, u32),
    ) -> io::Result<()> {
        let mut last = 0;
        let mut cursor_seen = (0, 0);
        let mut led_seen = (0, 0);
        loop {
            let (frame, seq, cursor, cs, leds, ls) = self.snapshot();
            let mut s = state.lock().unwrap();
            if s.closed {
                return Ok(());
            }
            let Some(request) = s.request else {
                drop(s);
                std::thread::park_timeout(Duration::from_millis(10));
                continue;
            };
            let changed = seq != last
                || cursor_seen != (cs, s.encoding_seq)
                || led_seen != (ls, s.encoding_seq);
            if request[0] != 0 && !changed {
                drop(s);
                std::thread::park_timeout(Duration::from_millis(10));
                continue;
            }
            let format = s.format.clone();
            let encodings = s.encodings.clone();
            let es = s.encoding_seq;
            s.request = None;
            drop(s);
            let mut out = vec![0, 0, 0, 0];
            let mut count = 0u16;
            if size != (frame.width, frame.height) {
                if !encodings.contains(&DESKTOP_SIZE) {
                    return Err(invalid("RFB client cannot resize"));
                }
                rect(
                    &mut out,
                    0,
                    0,
                    frame.width as u16,
                    frame.height as u16,
                    DESKTOP_SIZE,
                );
                count += 1;
                size = (frame.width, frame.height);
            } else {
                let x = (be16(&request[1..3]) as u32).min(frame.width);
                let y = (be16(&request[3..5]) as u32).min(frame.height);
                let w = (be16(&request[5..7]) as u32).min(frame.width - x);
                let h = (be16(&request[7..9]) as u32).min(frame.height - y);
                if w != 0 && h != 0 {
                    let pixels = self.cpu_pixels(&frame)?;
                    rect(&mut out, x as u16, y as u16, w as u16, h as u16, RAW);
                    count += 1;
                    for row in y..y + h {
                        for col in x..x + w {
                            let i = row as usize * frame.stride as usize + col as usize * 4;
                            format.pixel(&mut out, &pixels[i..i + 4]);
                        }
                    }
                }
            }
            if cursor_seen != (cs, es) {
                let alpha = encodings.contains(&ALPHA_CURSOR);
                let rich = encodings.contains(&RICH_CURSOR);
                if alpha || rich {
                    let (w, h) = if cursor.visible {
                        (cursor.width, cursor.height)
                    } else {
                        (0, 0)
                    };
                    rect(
                        &mut out,
                        cursor.hot_x as u16,
                        cursor.hot_y as u16,
                        w as u16,
                        h as u16,
                        if alpha { ALPHA_CURSOR } else { RICH_CURSOR },
                    );
                    count += 1;
                    if alpha {
                        out.extend(RAW.to_be_bytes());
                        if cursor.visible {
                            out.extend(&cursor.argb);
                        }
                    } else if cursor.visible {
                        for px in cursor.argb.as_chunks::<4>().0 {
                            format.pixel(&mut out, px);
                        }
                        let stride = (w as usize).div_ceil(8);
                        let mut mask = vec![0; stride * h as usize];
                        for y in 0..h as usize {
                            for x in 0..w as usize {
                                if cursor.argb[(y * w as usize + x) * 4 + 3] != 0 {
                                    mask[y * stride + x / 8] |= 128 >> (x % 8);
                                }
                            }
                        }
                        out.extend(mask);
                    }
                }
                cursor_seen = (cs, es);
            }
            if led_seen != (ls, es) {
                if encodings.contains(&LED_STATE) {
                    rect(&mut out, 0, 0, 0, 0, LED_STATE);
                    out.push(leds as u8);
                    count += 1;
                }
                led_seen = (ls, es);
            }
            out[2..4].copy_from_slice(&count.to_be_bytes());
            stream.write_all(&out)?;
            last = seq;
        }
    }
}

/// Map physical US-layout X11 keysyms onto QEMU qcode names.
/// Unknown keysyms are ignored rather than guessed.
pub fn keysym_to_qcode(key: u32) -> Option<&'static str> {
    Some(match key {
        0x41..=0x5a => return keysym_to_qcode(key + 32),
        0x61 => "a",
        0x62 => "b",
        0x63 => "c",
        0x64 => "d",
        0x65 => "e",
        0x66 => "f",
        0x67 => "g",
        0x68 => "h",
        0x69 => "i",
        0x6a => "j",
        0x6b => "k",
        0x6c => "l",
        0x6d => "m",
        0x6e => "n",
        0x6f => "o",
        0x70 => "p",
        0x71 => "q",
        0x72 => "r",
        0x73 => "s",
        0x74 => "t",
        0x75 => "u",
        0x76 => "v",
        0x77 => "w",
        0x78 => "x",
        0x79 => "y",
        0x7a => "z",
        0x30 | 0x29 => "0",
        0x31 | 0x21 => "1",
        0x32 | 0x40 => "2",
        0x33 | 0x23 => "3",
        0x34 | 0x24 => "4",
        0x35 | 0x25 => "5",
        0x36 | 0x5e => "6",
        0x37 | 0x26 => "7",
        0x38 | 0x2a => "8",
        0x39 | 0x28 => "9",
        0x20 => "spc",
        0x2d | 0x5f => "minus",
        0x3d | 0x2b => "equal",
        0x5b | 0x7b => "bracket_left",
        0x5d | 0x7d => "bracket_right",
        0x5c | 0x7c => "backslash",
        0x3b | 0x3a => "semicolon",
        0x27 | 0x22 => "apostrophe",
        0x60 | 0x7e => "grave_accent",
        0x2c | 0x3c => "comma",
        0x2e | 0x3e => "dot",
        0x2f | 0x3f => "slash",
        0xff08 => "backspace",
        0xff09 | 0xfe20 => "tab",
        0xff0d => "ret",
        0xff1b => "esc",
        0xffff => "delete",
        0xff63 => "insert",
        0xff50 => "home",
        0xff57 => "end",
        0xff55 => "pgup",
        0xff56 => "pgdn",
        0xff51 => "left",
        0xff52 => "up",
        0xff53 => "right",
        0xff54 => "down",
        0xffe1 => "shift",
        0xffe2 => "shift_r",
        0xffe3 => "ctrl",
        0xffe4 => "ctrl_r",
        0xffe9 => "alt",
        0xffea | 0xfe03 => "alt_r",
        0xffeb | 0xffe7 => "meta_l",
        0xffec | 0xffe8 => "meta_r",
        0xff67 => "menu",
        0xffe5 => "caps_lock",
        0xff7f => "num_lock",
        0xff14 => "scroll_lock",
        0xff13 => "pause",
        0xff61 | 0xff15 => "print",
        0xffbe => "f1",
        0xffbf => "f2",
        0xffc0 => "f3",
        0xffc1 => "f4",
        0xffc2 => "f5",
        0xffc3 => "f6",
        0xffc4 => "f7",
        0xffc5 => "f8",
        0xffc6 => "f9",
        0xffc7 => "f10",
        0xffc8 => "f11",
        0xffc9 => "f12",
        0xffb0 | 0xff9e => "kp_0",
        0xffb1 | 0xff9c => "kp_1",
        0xffb2 | 0xff99 => "kp_2",
        0xffb3 | 0xff9b => "kp_3",
        0xffb4 | 0xff96 => "kp_4",
        0xffb5 | 0xff9d => "kp_5",
        0xffb6 | 0xff98 => "kp_6",
        0xffb7 | 0xff95 => "kp_7",
        0xffb8 | 0xff97 => "kp_8",
        0xffb9 | 0xff9a => "kp_9",
        0xffae | 0xff9f => "kp_decimal",
        0xff8d => "kp_enter",
        0xffaa => "kp_multiply",
        0xffab => "kp_add",
        0xffad => "kp_subtract",
        0xffaf => "kp_divide",
        _ => return None,
    })
}
