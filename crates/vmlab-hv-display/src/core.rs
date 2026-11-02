use crate::{
    Capabilities, FrameFlags, Message, Rect, SurfaceKind, invalid, protocol, receive, send,
};
use std::collections::BTreeMap;
use std::fs::File;
use std::io;
use std::net::Shutdown;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::{
    fs::FileExt,
    net::{UnixListener, UnixStream},
};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex, Weak};
use std::time::Duration;

/// Immutable snapshots or reusable allocations protected by publication leases.
#[derive(Clone)]
pub enum Pixels {
    Shm(Arc<Vec<u8>>),
    Dmabuf {
        fd: Arc<OwnedFd>,
        acquire: Option<Arc<OwnedFd>>,
    },
    /// A reusable allocation, lent only while its publication lease is valid.
    Leased {
        buffers: Arc<Vec<Arc<OwnedFd>>>,
        buffer: u32,
        acquire: Option<Arc<OwnedFd>>,
        lease: Arc<crate::BufferLease>,
        readback: Arc<dyn Fn() -> io::Result<Vec<u8>> + Send + Sync>,
        cpu: Option<Arc<Vec<u8>>>,
    },
}

type DmaDescriptors<'a> = (Vec<&'a Arc<OwnedFd>>, u32, Option<&'a Arc<OwnedFd>>);

impl Pixels {
    fn dma(&self) -> Option<DmaDescriptors<'_>> {
        match self {
            Self::Shm(_) => None,
            Self::Dmabuf { fd, acquire } => Some((vec![fd], 0, acquire.as_ref())),
            Self::Leased {
                buffers,
                buffer,
                acquire,
                ..
            } => Some((buffers.iter().collect(), *buffer, acquire.as_ref())),
        }
    }
    pub(crate) fn pin(&self) -> Option<Option<crate::BufferGuard>> {
        match self {
            Self::Leased { lease, .. } => lease.acquire().map(Some),
            _ => Some(None),
        }
    }
}

#[derive(Clone)]
pub struct Frame {
    pub flags: FrameFlags,
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
            || ![
                protocol::XRGB8888,
                protocol::ARGB8888,
                protocol::XBGR8888,
                protocol::ABGR8888,
            ]
            .contains(&self.drm_fourcc)
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
        if let Pixels::Leased {
            buffers, buffer, ..
        } = &self.pixels
            && (buffers.is_empty() || buffers.len() > 16 || *buffer as usize >= buffers.len())
        {
            return Err(invalid("invalid dma-buf pool"));
        }
        Ok(())
    }
}

/// Device integration supplies snapshots and synchronized CPU readback.
/// Immutable dma-bufs stay unchanged while any Frame Arc exists. Leased buffers
/// instead exclude writes until all sampling guards have been released.
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
    revision: u64,
}
struct Shared {
    closed: Arc<AtomicBool>,
    sessions: Mutex<Vec<Weak<UnixStream>>>,
    capture: Mutex<()>,
    state: Mutex<State>,
    changed: Condvar,
    source: Arc<dyn FrameSource>,
    cpu_clients: AtomicUsize,
    input: Arc<crate::input::InputQueue>,
    input_worker: Mutex<Option<std::thread::JoinHandle<()>>>,
}
impl Drop for Shared {
    fn drop(&mut self) {
        self.input.close();
    }
}
#[derive(Clone)]
pub struct DisplayServer {
    shared: Arc<Shared>,
}
impl DisplayServer {
    pub fn new(source: Arc<dyn FrameSource>, sink: Arc<dyn InputSink>) -> io::Result<Self> {
        let frame = source.frame()?;
        frame.validate()?;
        let closed = Arc::new(AtomicBool::new(false));
        let input = Arc::new(crate::input::InputQueue::default());
        let messages = input.clone();
        let input_worker = std::thread::Builder::new()
            .name("display-input".into())
            .spawn(move || {
                while let Some(message) = messages.pop() {
                    sink.input(message);
                }
            })?;
        Ok(Self {
            shared: Arc::new(Shared {
                closed,
                sessions: Mutex::new(Vec::new()),
                capture: Mutex::new(()),
                state: Mutex::new(State {
                    frame: Arc::new(frame),
                    seq: 1,
                    cursor: Cursor::default(),
                    cursor_seq: 1,
                    leds: 0,
                    led_seq: 1,
                    revision: 0,
                }),
                changed: Condvar::new(),
                source,
                cpu_clients: AtomicUsize::new(0),
                input,
                input_worker: Mutex::new(Some(input_worker)),
            }),
        })
    }
    /// Interrupt all sessions, including unfinished handshakes and blocked writes.
    pub fn shutdown(&self) {
        self.shared.closed.store(true, Ordering::SeqCst);
        self.shared.input.close();
        for stream in self
            .shared
            .sessions
            .lock()
            .unwrap()
            .drain(..)
            .filter_map(|s| s.upgrade())
        {
            let _ = stream.shutdown(Shutdown::Both);
        }
        {
            let _state = self.shared.state.lock().unwrap();
            self.shared.changed.notify_all();
        }
        if let Some(worker) = self.shared.input_worker.lock().unwrap().take()
            && worker.thread().id() != std::thread::current().id()
        {
            let _ = worker.join();
        }
    }

    pub(crate) fn session(&self, stream: &UnixStream) -> io::Result<Arc<UnixStream>> {
        let session = Arc::new(stream.try_clone()?);
        let mut sessions = self.shared.sessions.lock().unwrap();
        if self.shared.closed.load(Ordering::SeqCst) {
            return Err(io::Error::other("display server has stopped"));
        }
        sessions.retain(|session| session.strong_count() != 0);
        sessions.push(Arc::downgrade(&session));
        Ok(session)
    }

    /// Capture the source once. No socket or client lock is acquired here.
    pub fn publish(&self) -> io::Result<()> {
        self.capture(false)
    }

    /// Publish a device flush without calling back into the locked device.
    pub fn publish_frame(&self, frame: Frame) -> io::Result<()> {
        frame.validate()?;
        self.replace(frame, false, None)
    }

    pub fn needs_cpu(&self) -> bool {
        self.shared.cpu_clients.load(Ordering::SeqCst) != 0
    }

    pub(crate) fn cpu_client(&self) -> CpuClient {
        self.shared.cpu_clients.fetch_add(1, Ordering::SeqCst);
        CpuClient(self.clone())
    }

    fn capture(&self, force: bool) -> io::Result<()> {
        // Keep capture order and publication order identical for dirty sources.
        let _capture = self.shared.capture.lock().unwrap();
        let seq = self.shared.state.lock().unwrap().seq;
        let frame = self.shared.source.frame()?;
        frame.validate()?;
        self.replace(frame, force, Some(seq))
    }

    fn replace(&self, mut frame: Frame, force: bool, expected: Option<u64>) -> io::Result<()> {
        let mut state = self.shared.state.lock().unwrap();
        if expected.is_some_and(|seq| seq != state.seq) {
            return Ok(());
        }
        let previous = &state.frame;
        if !force
            && frame.damage.is_empty()
            && (
                frame.generation,
                frame.width,
                frame.height,
                frame.stride,
                frame.drm_fourcc,
                frame.modifier,
                frame.flags,
            ) == (
                previous.generation,
                previous.width,
                previous.height,
                previous.stride,
                previous.drm_fourcc,
                previous.modifier,
                previous.flags,
            )
            && std::mem::discriminant(&frame.pixels) == std::mem::discriminant(&previous.pixels)
        {
            return Ok(());
        }
        if frame.flags != previous.flags {
            frame.damage = vec![frame.full()];
        }
        state.frame = Arc::new(frame);
        state.seq += 1;
        state.revision += 1;
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
        if state.cursor == cursor {
            return Ok(());
        }
        state.cursor = cursor;
        state.cursor_seq += 1;
        state.revision += 1;
        self.shared.changed.notify_all();
        Ok(())
    }
    pub fn set_leds(&self, mask: u32) {
        let mut state = self.shared.state.lock().unwrap();
        if state.leds == mask & 7 {
            return;
        }
        state.leds = mask & 7;
        state.led_seq += 1;
        state.revision += 1;
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
    pub(crate) fn input(&self, message: Message) -> io::Result<()> {
        self.shared.input.push(message)
    }
    pub(crate) fn cpu_pixels(&self, frame: &Frame) -> io::Result<Vec<u8>> {
        let mut bytes = match &frame.pixels {
            Pixels::Shm(b) => b.as_ref().clone(),
            Pixels::Dmabuf { .. } => self.shared.source.readback(frame)?,
            Pixels::Leased { cpu, readback, .. } => match cpu {
                Some(bytes) => bytes.as_ref().clone(),
                None => readback()?,
            },
        };
        if bytes.len() != frame.stride as usize * frame.height as usize {
            return Err(invalid("invalid readback length"));
        }
        if matches!(frame.drm_fourcc, protocol::XBGR8888 | protocol::ABGR8888) {
            for row in bytes.chunks_mut(frame.stride as usize) {
                for pixel in row[..frame.width as usize * 4].as_chunks_mut::<4>().0 {
                    pixel.swap(0, 2);
                }
            }
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
        let _session = self.session(&stream)?;
        let Message::Hello {
            version: protocol::VERSION,
            capabilities,
        } = receive(&stream)?.message
        else {
            return Err(invalid("expected display v1 Hello"));
        };
        let caps = Capabilities(capabilities.0 & Capabilities::ALL.0);
        let _cpu = (caps.has(Capabilities::FRAMES) && !caps.has(Capabilities::DMABUF_OK))
            .then(|| self.cpu_client());
        if caps.has(Capabilities::FRAMES) {
            self.publish()?;
        }
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
            {
                let mut local = state.lock().unwrap();
                local.closed = true;
                local.revision += 1;
            }
            let _state = server.shared.state.lock().unwrap();
            server.shared.changed.notify_all();
            let _ = reader.shutdown(Shutdown::Both);
            result
        });
        let result = self.native_frames(&stream, caps, &local);
        let _ = stream.shutdown(Shutdown::Both);
        let input = handle
            .join()
            .map_err(|_| io::Error::other("display input thread panicked"))?;
        result?;
        match input {
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::UnexpectedEof
                        | io::ErrorKind::BrokenPipe
                        | io::ErrorKind::ConnectionReset
                        | io::ErrorKind::ConnectionAborted
                ) =>
            {
                Ok(())
            }
            result => result,
        }
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
                    if local.lock().unwrap().surface != surface_id {
                        return Err(invalid("CpuCopy names stale surface"));
                    }
                    if matches!(
                        self.shared.state.lock().unwrap().frame.pixels,
                        Pixels::Leased { .. }
                    ) {
                        self.shared.state.lock().unwrap().seq += 1;
                    } else {
                        self.capture(true)?;
                    }
                    local.lock().unwrap().cpu_copy = true;
                }
                Message::Key { .. }
                | Message::PointerAbs { .. }
                | Message::Buttons { .. }
                | Message::Scroll { .. }
                    if caps.has(Capabilities::INPUT) =>
                {
                    self.input(msg)?;
                    continue;
                }
                Message::RequestSize { width, height }
                    if caps.has(Capabilities::INPUT)
                        && (1..=16384).contains(&width)
                        && (1..=16384).contains(&height) =>
                {
                    self.input(msg)?;
                    continue;
                }
                _ => return Err(invalid("unexpected client message or missing capability")),
            }
            local.lock().unwrap().revision += 1;
            let _state = self.shared.state.lock().unwrap();
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
        let mut dma_buffers: Vec<Arc<OwnedFd>> = Vec::new();
        let mut geometry = None;
        let mut surface = 0;
        let mut last = 0;
        let mut cursor_seen = 0;
        let mut led_seen = 0;
        let mut cpu_mode = !caps.has(Capabilities::DMABUF_OK);
        let mut cpu_client = None;
        loop {
            let (closed, cpu_copy, can_send_frame, local_revision) = {
                let s = local.lock().unwrap();
                (
                    s.closed,
                    s.cpu_copy,
                    caps.has(Capabilities::FRAMES) && s.held.len() < 3,
                    s.revision,
                )
            };
            // Keep metadata flowing when the ring is full without pinning skipped pixels.
            let (frame, seq, cursor, cs, leds, ls, revision) = {
                let s = self.shared.state.lock().unwrap();
                (
                    can_send_frame.then(|| s.frame.clone()),
                    s.seq,
                    s.cursor.clone(),
                    s.cursor_seq,
                    s.leds,
                    s.led_seq,
                    s.revision,
                )
            };
            if closed || self.shared.closed.load(Ordering::SeqCst) {
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
            if let Some(frame) = frame
                && (seq != last || cpu_copy)
            {
                cpu_mode |= cpu_copy;
                if cpu_copy && cpu_client.is_none() && caps.has(Capabilities::DMABUF_OK) {
                    cpu_client = Some(self.cpu_client());
                }
                let dma =
                    frame.pixels.dma().is_some() && caps.has(Capabilities::DMABUF_OK) && !cpu_mode;
                let geom = (
                    frame.generation,
                    frame.width,
                    frame.height,
                    frame.stride,
                    if !dma && frame.drm_fourcc == protocol::XBGR8888 {
                        protocol::XRGB8888
                    } else if !dma && frame.drm_fourcc == protocol::ABGR8888 {
                        protocol::ARGB8888
                    } else {
                        frame.drm_fourcc
                    },
                    if dma { frame.modifier } else { 0 },
                    dma,
                );
                let pin = if !dma && matches!(&frame.pixels, Pixels::Leased { .. }) {
                    Some(None)
                } else {
                    frame.pixels.pin()
                };
                let Some(pin) = pin else {
                    std::thread::park_timeout(Duration::from_millis(1));
                    continue;
                };
                let mut s = local.lock().unwrap();
                // Old dma-buf Frames stay alive until Release, even after reconfigure.
                if s.held.len() < 3 {
                    let stable = caps.has(Capabilities::STABLE_BUFFERS);
                    let buffers_changed = dma
                        && frame.pixels.dma().is_some_and(|(buffers, _, _)| {
                            buffers.len() != dma_buffers.len()
                                || buffers
                                    .iter()
                                    .zip(&dma_buffers)
                                    .any(|(a, b)| !Arc::ptr_eq(a, b))
                        });
                    let reconfigure =
                        geometry != Some(geom) || (dma && (!stable || buffers_changed));
                    if reconfigure {
                        surface += 1;
                        s.surface = surface;
                        ring.clear();
                        dma_buffers = if dma {
                            frame.pixels.dma().unwrap().0.into_iter().cloned().collect()
                        } else {
                            Vec::new()
                        };
                        if !dma {
                            for _ in 0..3 {
                                ring.push(memfd(frame.stride as u64 * frame.height as u64)?);
                            }
                        }
                        geometry = Some(geom);
                    }
                    let index = if dma {
                        Some(if stable {
                            frame.pixels.dma().unwrap().1
                        } else {
                            0
                        })
                    } else {
                        (0..3).find(|i| !s.held.keys().any(|(sid, b, _)| *sid == surface && b == i))
                    };
                    if let Some(index) = index {
                        // Only dma-buf clients retain a producer lease through Release.
                        // CPU readback below owns its own transient lease.
                        let (dma_pin, cpu_pin) = if dma { (pin, None) } else { (None, pin) };
                        s.held
                            .insert((surface, index, seq), (frame.clone(), dma_pin));
                        drop(s);
                        if reconfigure {
                            let fds = if dma {
                                let (buffers, buffer, _) = frame.pixels.dma().unwrap();
                                if stable {
                                    buffers.iter().map(|fd| fd.as_raw_fd()).collect()
                                } else {
                                    vec![buffers[buffer as usize].as_raw_fd()]
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
                                    drm_fourcc: geom.4,
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
                            // Reset can destroy readback resources after this frame was captured.
                            let pixels = match self.cpu_pixels(&frame) {
                                Ok(pixels) => pixels,
                                Err(error)
                                    if error.kind() == io::ErrorKind::WouldBlock
                                        || self.shared.state.lock().unwrap().frame.generation
                                            != frame.generation =>
                                {
                                    local.lock().unwrap().held.remove(&(surface, index, seq));
                                    continue;
                                }
                                Err(error) => return Err(error),
                            };
                            ring[index as usize].write_all_at(&pixels, 0)?;
                            drop(cpu_pin);
                            // Shm has its own immutable storage; it need not stall the guest.
                        }
                        let sync = if dma {
                            frame
                                .pixels
                                .dma()
                                .unwrap()
                                .2
                                .map(|fd| vec![fd.as_raw_fd()])
                                .unwrap_or_default()
                        } else {
                            vec![]
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
                                flags: if caps.has(Capabilities::FRAME_FLAGS) {
                                    frame.flags
                                } else {
                                    FrameFlags::default()
                                },
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
            // The reader drops its local lock before taking state to notify.
            // Checking both revisions under state prevents lost wakeups on Release.
            let state = self.shared.state.lock().unwrap();
            if state.revision == revision
                && local.lock().unwrap().revision == local_revision
                && !self.shared.closed.load(Ordering::SeqCst)
            {
                drop(self.shared.changed.wait(state).unwrap());
            }
        }
    }
}
#[derive(Default)]
struct ClientState {
    held: BTreeMap<(u64, u32, u64), HeldFrame>,
    closed: bool,
    cpu_copy: bool,
    surface: u64,
    revision: u64,
}
type HeldFrame = (Arc<Frame>, Option<crate::BufferGuard>);

pub(crate) struct CpuClient(DisplayServer);
impl Drop for CpuClient {
    fn drop(&mut self) {
        self.0.shared.cpu_clients.fetch_sub(1, Ordering::SeqCst);
    }
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
