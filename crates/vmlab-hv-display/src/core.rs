use crate::{Capabilities, Message, Rect, SurfaceKind, invalid, protocol, receive, send};
use std::collections::BTreeMap;
use std::fs::File;
use std::io;
use std::net::Shutdown;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::{
    fs::FileExt,
    net::{UnixListener, UnixStream},
};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

/// Published pixels must stay immutable for the lifetime of their Arc.
#[derive(Clone)]
pub enum Pixels {
    Shm(Arc<Vec<u8>>),
    Dmabuf {
        fd: Arc<OwnedFd>,
        acquire: Option<Arc<OwnedFd>>,
    },
}

#[derive(Clone)]
pub struct Frame {
    /// Increment on reset or same-sized scanout replacement to force configure.
    pub generation: u64,
    pub width: u32,
    pub height: u32,
    pub stride: u32,
    pub drm_fourcc: u32,
    pub modifier: u64,
    pub timestamp_ns: u64,
    pub damage: Vec<Rect>,
    pub pixels: Pixels,
}
impl Frame {
    pub fn full(&self) -> Rect {
        Rect {
            x: 0,
            y: 0,
            width: self.width,
            height: self.height,
        }
    }
    fn validate(&self) -> io::Result<()> {
        if self.width == 0
            || self.height == 0
            || self.width > 16384
            || self.height > 16384
            || self.stride < self.width * 4
            || self.stride as u64 * self.height as u64 > 256 * 1024 * 1024
            || ![protocol::XRGB8888, protocol::ARGB8888].contains(&self.drm_fourcc)
            || self.damage.len() > 65535
            || self.damage.iter().any(|r| {
                r.x as u64 + r.width as u64 > self.width as u64
                    || r.y as u64 + r.height as u64 > self.height as u64
            })
        {
            return Err(invalid("invalid frame geometry, format, or damage"));
        }
        if let Pixels::Shm(bytes) = &self.pixels
            && (bytes.len() != self.stride as usize * self.height as usize || self.modifier != 0)
        {
            return Err(invalid("invalid shm pixels"));
        }
        Ok(())
    }
}

/// Device integration supplies immutable snapshots and synchronized CPU readback.
/// A dma-buf producer must retain its allocation without rewriting it until all
/// Arcs to the published Frame have gone away. Readback must wait on acquire.
pub trait FrameSource: Send + Sync + 'static {
    fn frame(&self) -> io::Result<Frame>;
    fn readback(&self, frame: &Frame) -> io::Result<Vec<u8>>;
}
pub trait InputSink: Send + Sync + 'static {
    fn input(&self, message: Message);
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Cursor {
    pub width: u32,
    pub height: u32,
    pub hot_x: u32,
    pub hot_y: u32,
    /// Premultiplied ARGB words in little-endian order, bytes B G R A.
    pub argb: Vec<u8>,
    pub x: i32,
    pub y: i32,
    pub visible: bool,
}
pub(crate) struct State {
    pub frame: Arc<Frame>,
    pub seq: u64,
    pub cursor: Cursor,
    pub cursor_seq: u64,
    pub leds: u32,
    pub led_seq: u64,
}
struct Shared {
    state: Mutex<State>,
    changed: Condvar,
    source: Arc<dyn FrameSource>,
    sink: Arc<dyn InputSink>,
}
#[derive(Clone)]
pub struct DisplayServer {
    shared: Arc<Shared>,
}
impl DisplayServer {
    pub fn new(source: Arc<dyn FrameSource>, sink: Arc<dyn InputSink>) -> io::Result<Self> {
        let frame = source.frame()?;
        frame.validate()?;
        Ok(Self {
            shared: Arc::new(Shared {
                state: Mutex::new(State {
                    frame: Arc::new(frame),
                    seq: 1,
                    cursor: Cursor::default(),
                    cursor_seq: 1,
                    leds: 0,
                    led_seq: 1,
                }),
                changed: Condvar::new(),
                source,
                sink,
            }),
        })
    }
    /// Capture the source once. No socket or client lock is acquired here.
    pub fn publish(&self) -> io::Result<()> {
        let frame = self.shared.source.frame()?;
        frame.validate()?;
        let mut state = self.shared.state.lock().unwrap();
        state.frame = Arc::new(frame);
        state.seq += 1;
        self.shared.changed.notify_all();
        Ok(())
    }
    pub fn set_cursor(&self, cursor: Cursor) -> io::Result<()> {
        Message::decode(
            &Message::CursorShape {
                width: cursor.width,
                height: cursor.height,
                hot_x: cursor.hot_x,
                hot_y: cursor.hot_y,
                argb: cursor.argb.clone(),
            }
            .encode()?,
        )?;
        let mut state = self.shared.state.lock().unwrap();
        state.cursor = cursor;
        state.cursor_seq += 1;
        self.shared.changed.notify_all();
        Ok(())
    }
    pub fn set_leds(&self, mask: u32) {
        let mut state = self.shared.state.lock().unwrap();
        state.leds = mask & 7;
        state.led_seq += 1;
        self.shared.changed.notify_all();
    }
    pub(crate) fn snapshot(&self) -> (Arc<Frame>, u64, Cursor, u64, u32, u64) {
        let s = self.shared.state.lock().unwrap();
        (
            s.frame.clone(),
            s.seq,
            s.cursor.clone(),
            s.cursor_seq,
            s.leds,
            s.led_seq,
        )
    }
    pub(crate) fn input(&self, message: Message) {
        self.shared.sink.input(message);
    }
    pub(crate) fn cpu_pixels(&self, frame: &Frame) -> io::Result<Vec<u8>> {
        let bytes = match &frame.pixels {
            Pixels::Shm(b) => b.as_ref().clone(),
            Pixels::Dmabuf { .. } => self.shared.source.readback(frame)?,
        };
        if bytes.len() != frame.stride as usize * frame.height as usize {
            return Err(invalid("invalid readback length"));
        }
        Ok(bytes)
    }
    /// Accept until the supplied listener fails. Each connection has its own worker.
    /// The caller owns socket binding, permissions, and listener lifetime.
    pub fn serve(&self, listener: UnixListener, rfb: bool) -> io::Result<()> {
        for stream in listener.incoming() {
            let stream = stream?;
            let server = self.clone();
            std::thread::spawn(move || {
                let _ = if rfb {
                    server.serve_rfb(stream)
                } else {
                    server.serve_native(stream)
                };
            });
        }
        Ok(())
    }
    pub fn serve_native(&self, stream: UnixStream) -> io::Result<()> {
        let Message::Hello {
            version: protocol::VERSION,
            capabilities,
        } = receive(&stream)?.message
        else {
            return Err(invalid("expected display v1 Hello"));
        };
        let caps = Capabilities(capabilities.0 & Capabilities::ALL.0);
        send(
            &stream,
            &Message::Hello {
                version: protocol::VERSION,
                capabilities: caps,
            },
            &[],
        )?;
        let local = Arc::new(Mutex::new(ClientState::default()));
        let reader = stream.try_clone()?;
        let state = local.clone();
        let server = self.clone();
        let handle = std::thread::spawn(move || {
            let result = server.native_input(&reader, caps, &state);
            state.lock().unwrap().closed = true;
            server.shared.changed.notify_all();
            let _ = reader.shutdown(Shutdown::Both);
            result
        });
        let result = self.native_frames(&stream, caps, &local);
        let _ = stream.shutdown(Shutdown::Both);
        let _ = handle.join();
        result
    }
    fn native_input(
        &self,
        stream: &UnixStream,
        caps: Capabilities,
        local: &Mutex<ClientState>,
    ) -> io::Result<()> {
        loop {
            let msg = receive(stream)?.message;
            match msg {
                Message::Release {
                    surface_id,
                    buffer,
                    seq,
                } if caps.has(Capabilities::FRAMES) => {
                    let mut s = local.lock().unwrap();
                    if s.held.remove(&(surface_id, buffer, seq)).is_none() {
                        return Err(invalid("release does not match a held frame"));
                    }
                }
                Message::CpuCopy { surface_id } if caps.has(Capabilities::CPU_COPY) => {
                    let mut s = local.lock().unwrap();
                    if s.surface != surface_id {
                        return Err(invalid("CpuCopy names stale surface"));
                    }
                    s.cpu_copy = true;
                }
                Message::Key { .. }
                | Message::PointerAbs { .. }
                | Message::Buttons { .. }
                | Message::Scroll { .. }
                    if caps.has(Capabilities::INPUT) =>
                {
                    self.input(msg)
                }
                Message::RequestSize { .. } => return Err(invalid("RequestSize is reserved")),
                _ => return Err(invalid("unexpected client message or missing capability")),
            }
            self.shared.changed.notify_all();
        }
    }
    fn native_frames(
        &self,
        stream: &UnixStream,
        caps: Capabilities,
        local: &Mutex<ClientState>,
    ) -> io::Result<()> {
        let mut ring: Vec<File> = Vec::new();
        let mut geometry = None;
        let mut surface = 0;
        let mut last = 0;
        let mut cursor_seen = 0;
        let mut led_seen = 0;
        let mut cpu_mode = !caps.has(Capabilities::DMABUF_OK);
        loop {
            let (frame, seq, cursor, cs, leds, ls) = self.snapshot();
            let (closed, cpu_copy) = {
                let s = local.lock().unwrap();
                (s.closed, s.cpu_copy)
            };
            if closed {
                return Ok(());
            }
            if caps.has(Capabilities::CURSOR) && cs != cursor_seen {
                send(
                    stream,
                    &Message::CursorShape {
                        width: cursor.width,
                        height: cursor.height,
                        hot_x: cursor.hot_x,
                        hot_y: cursor.hot_y,
                        argb: cursor.argb,
                    },
                    &[],
                )?;
                send(
                    stream,
                    &Message::CursorState {
                        x: cursor.x,
                        y: cursor.y,
                        visible: cursor.visible,
                    },
                    &[],
                )?;
                cursor_seen = cs;
            }
            if caps.has(Capabilities::INPUT) && ls != led_seen {
                send(stream, &Message::LedState { mask: leds }, &[])?;
                led_seen = ls;
            }
            if caps.has(Capabilities::FRAMES) && (seq != last || cpu_copy) {
                cpu_mode |= cpu_copy;
                let dma = matches!(frame.pixels, Pixels::Dmabuf { .. })
                    && caps.has(Capabilities::DMABUF_OK)
                    && !cpu_mode;
                let geom = (
                    frame.generation,
                    frame.width,
                    frame.height,
                    frame.stride,
                    frame.drm_fourcc,
                    if dma { frame.modifier } else { 0 },
                    dma,
                );
                let mut s = local.lock().unwrap();
                // Old dma-buf Frames stay alive until Release, even after reconfigure.
                if s.held.len() < 3 {
                    let reconfigure = geometry != Some(geom) || dma;
                    if reconfigure {
                        surface += 1;
                        s.surface = surface;
                        ring.clear();
                        if !dma {
                            for _ in 0..3 {
                                ring.push(memfd(frame.stride as u64 * frame.height as u64)?);
                            }
                        }
                        geometry = Some(geom);
                    }
                    let index = if dma {
                        Some(0)
                    } else {
                        (0..3).find(|i| !s.held.keys().any(|(sid, b, _)| *sid == surface && b == i))
                    };
                    if let Some(index) = index {
                        s.held.insert((surface, index, seq), frame.clone());
                        drop(s);
                        if reconfigure {
                            let fds = if let Pixels::Dmabuf { fd, .. } = &frame.pixels {
                                if dma {
                                    vec![fd.as_raw_fd()]
                                } else {
                                    ring.iter().map(AsRawFd::as_raw_fd).collect()
                                }
                            } else {
                                ring.iter().map(AsRawFd::as_raw_fd).collect()
                            };
                            send(
                                stream,
                                &Message::SurfaceConfigure {
                                    surface_id: surface,
                                    width: frame.width,
                                    height: frame.height,
                                    drm_fourcc: frame.drm_fourcc,
                                    stride: frame.stride,
                                    modifier: geom.5,
                                    kind: if dma {
                                        SurfaceKind::Dmabuf
                                    } else {
                                        SurfaceKind::Shm
                                    },
                                    buffers: fds.len() as u32,
                                },
                                &fds,
                            )?;
                        }
                        if !dma {
                            ring[index as usize].write_all_at(&self.cpu_pixels(&frame)?, 0)?;
                        }
                        let sync = match &frame.pixels {
                            Pixels::Dmabuf {
                                acquire: Some(fd), ..
                            } if dma => vec![fd.as_raw_fd()],
                            _ => vec![],
                        };
                        // A skipped frame may have damaged pixels outside the latest damage list.
                        let damage = if reconfigure || seq != last + 1 || cpu_copy {
                            vec![frame.full()]
                        } else {
                            frame.damage.clone()
                        };
                        send(
                            stream,
                            &Message::FrameReady {
                                surface_id: surface,
                                buffer: index,
                                seq,
                                timestamp_ns: frame.timestamp_ns,
                                damage,
                            },
                            &sync,
                        )?;
                        last = seq;
                        // Readback remains selected for this connection.
                        if cpu_copy {
                            local.lock().unwrap().cpu_copy = false;
                        }
                    }
                }
            }
            // Bounded wait also handles a Release racing with the snapshot above.
            let state = self.shared.state.lock().unwrap();
            let _ = self
                .shared
                .changed
                .wait_timeout(state, Duration::from_millis(10))
                .unwrap();
        }
    }
}
#[derive(Default)]
struct ClientState {
    held: BTreeMap<(u64, u32, u64), Arc<Frame>>,
    closed: bool,
    cpu_copy: bool,
    surface: u64,
}
fn memfd(size: u64) -> io::Result<File> {
    // SAFETY: The name is NUL-terminated and flags are valid for memfd_create.
    let fd = unsafe {
        libc::memfd_create(
            c"vmlab-frame".as_ptr(),
            libc::MFD_CLOEXEC | libc::MFD_ALLOW_SEALING,
        )
    };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: memfd_create returned a new descriptor, now owned by File.
    let file = unsafe { File::from_raw_fd(fd) };
    file.set_len(size)?;
    // SAFETY: fcntl operates on a live file. Size seals prevent client truncation.
    if unsafe {
        libc::fcntl(
            fd,
            libc::F_ADD_SEALS,
            libc::F_SEAL_GROW | libc::F_SEAL_SHRINK | libc::F_SEAL_SEAL,
        )
    } < 0
    {
        return Err(io::Error::last_os_error());
    }
    Ok(file)
}
