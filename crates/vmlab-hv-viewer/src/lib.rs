//! Headless display state and input translation. The binary owns the window and GPU.

pub mod clipboard;
mod connection;
mod grab;
pub mod input;
pub mod render;
pub mod shell;

use std::collections::HashMap;
use std::fs::File;
use std::io;
use std::os::fd::AsRawFd;
use std::os::unix::fs::FileExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};
use vmlab_hv_display::{Capabilities, Client, FrameFlags, Message, Packet, Rect, SurfaceKind};

pub fn invalid(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

pub fn shm_only(forced: bool, wslg_exists: bool, wayland: Option<&str>) -> bool {
    forced || wslg_exists || wayland.is_some_and(|path| path.starts_with("/mnt/wslg/"))
}

#[derive(Debug, Clone)]
pub struct Options {
    pub display: PathBuf,
    pub lab_socket: Option<PathBuf>,
    pub machine: Option<String>,
    pub release_chord: String,
    pub shm: bool,
}

impl Options {
    pub const HELP: &str = "vmlab-hv-viewer --display PATH [--lab-socket PATH --machine NAME] [--release-chord ctrl+alt] [--shm]";

    pub fn parse(args: impl IntoIterator<Item = String>) -> anyhow::Result<Self> {
        let mut args = args.into_iter();
        let (mut display, mut lab_socket, mut machine) = (None, None, None);
        let mut release_chord = "ctrl+alt".to_owned();
        let mut shm = false;
        while let Some(arg) = args.next() {
            match arg.as_str() {
                "--display" => {
                    display = Some(PathBuf::from(
                        args.next()
                            .ok_or_else(|| invalid("--display needs a path"))?,
                    ))
                }
                "--lab-socket" => {
                    lab_socket = Some(PathBuf::from(
                        args.next()
                            .ok_or_else(|| invalid("--lab-socket needs a path"))?,
                    ))
                }
                "--machine" => {
                    machine = Some(
                        args.next()
                            .ok_or_else(|| invalid("--machine needs a name"))?,
                    )
                }
                "--release-chord" => {
                    release_chord = args.next().ok_or_else(|| {
                        invalid("--release-chord needs QEMU key names joined by +")
                    })?
                }
                "--shm" => shm = true,
                _ => anyhow::bail!("unknown argument {arg}; {}", Self::HELP),
            }
        }
        anyhow::ensure!(
            lab_socket.is_some() == machine.is_some(),
            "--lab-socket and --machine must be supplied together"
        );
        input::Input::new(&release_chord)?;
        Ok(Self {
            display: display.ok_or_else(|| invalid(Self::HELP))?,
            lab_socket,
            machine,
            release_chord,
            shm: shm_only(
                shm,
                Path::new("/mnt/wslg").exists(),
                std::env::var("WAYLAND_DISPLAY").ok().as_deref(),
            ),
        })
    }
}

pub struct Surface {
    pub id: u64,
    pub width: u32,
    pub height: u32,
    pub fourcc: u32,
    pub stride: u32,
    pub modifier: u64,
    pub kind: SurfaceKind,
    pub buffers: Vec<File>,
    /// Latest complete CPU image, independent of the server's rotating buffers.
    pub pixels: Vec<u8>,
    initialized: bool,
}

#[derive(Clone, Debug)]
pub struct Frame {
    pub surface: u64,
    pub buffer: u32,
    pub seq: u64,
    pub flags: FrameFlags,
    pub damage: Vec<Rect>,
}

#[derive(Default, Debug)]
pub struct Cursor {
    pub width: u32,
    pub height: u32,
    pub hot_x: u32,
    pub hot_y: u32,
    pub argb: Vec<u8>,
    pub x: i32,
    pub y: i32,
    pub visible: bool,
    pub generation: u64,
}

impl Cursor {
    pub fn has_shape(&self) -> bool {
        self.width != 0 && self.height != 0
    }

    /// A local arrow for host placeholder frames without a guest cursor shape.
    pub fn fallback() -> Self {
        let rows = [
            "X...........",
            "XX..........",
            "XOX.........",
            "XOOX........",
            "XOOOX.......",
            "XOOOOX......",
            "XOOOOOX.....",
            "XOOOOOOX....",
            "XOOOOOOOX...",
            "XOOOOXXXXX..",
            "XOOXOX......",
            "XOX.OX......",
            "XX..XOX.....",
            "X...XOX.....",
            ".....XX.....",
            "............",
        ];
        Self {
            width: 12,
            height: 16,
            visible: true,
            generation: u64::MAX,
            argb: rows
                .iter()
                .flat_map(|row| row.bytes())
                .flat_map(|pixel| match pixel {
                    b'X' => [0, 0, 0, 255],
                    b'O' => [255; 4],
                    _ => [0; 4],
                })
                .collect(),
            ..Self::default()
        }
    }
}

pub struct Core {
    pub client: Arc<Client>,
    pub surfaces: HashMap<u64, Surface>,
    pub current: Option<Frame>,
    pub cursor: Cursor,
    pub leds: u32,
    pub input: input::Input,
    resize: Option<(Instant, u32, u32)>,
}

impl Core {
    pub fn host_cursor_visible(&self, pointer_inside: bool) -> bool {
        !pointer_inside || self.current.is_none()
    }

    pub fn use_fallback_cursor(&self) -> bool {
        !self.cursor.has_shape()
            && self
                .current
                .as_ref()
                .is_some_and(|frame| frame.flags.has(FrameFlags::PLACEHOLDER))
    }

    pub fn connect(path: &Path, shm: bool, chord: &str) -> io::Result<Self> {
        let mut caps = Capabilities::ALL.0;
        if shm {
            caps &= !Capabilities::DMABUF_OK;
        }
        Self::new(Arc::new(Client::connect(path, Capabilities(caps))?), chord)
    }

    pub fn new(client: Arc<Client>, chord: &str) -> io::Result<Self> {
        if !client.capabilities.has(Capabilities::FRAMES) {
            return Err(invalid("server did not negotiate frames"));
        }
        Ok(Self {
            client,
            surfaces: HashMap::new(),
            current: None,
            cursor: Cursor::default(),
            leds: 0,
            input: input::Input::new(chord)?,
            resize: None,
        })
    }

    /// Returns a frame whose release is the caller's responsibility, after GPU use.
    pub fn receive(&mut self, packet: Packet) -> io::Result<Option<Frame>> {
        match packet.message {
            Message::SurfaceConfigure {
                surface_id,
                width,
                height,
                drm_fourcc,
                stride,
                modifier,
                kind,
                buffers,
            } => {
                if self.surfaces.keys().any(|id| *id >= surface_id) {
                    return Err(invalid("surface configure id did not increase"));
                }
                if width == 0
                    || height == 0
                    || width > 16384
                    || height > 16384
                    || stride < width * 4
                    || buffers == 0
                    || buffers > 16
                    || packet.fds.len() != buffers as usize
                    || ![
                        vmlab_hv_display::protocol::XRGB8888,
                        vmlab_hv_display::protocol::ARGB8888,
                    ]
                    .contains(&drm_fourcc)
                        && !(kind == SurfaceKind::Dmabuf
                            && [
                                vmlab_hv_display::protocol::XBGR8888,
                                vmlab_hv_display::protocol::ABGR8888,
                            ]
                            .contains(&drm_fourcc))
                {
                    return Err(invalid("invalid display surface"));
                }
                let size = u64::from(stride) * u64::from(height);
                if size > 256 << 20 {
                    return Err(invalid("display surface exceeds 256 MiB"));
                }
                let files: Vec<File> = packet.fds.into_iter().map(File::from).collect();
                if kind == SurfaceKind::Shm
                    && (modifier != 0
                        || files
                            .iter()
                            .any(|f| f.metadata().map_or(true, |m| m.len() < size)))
                {
                    return Err(invalid("invalid shared-memory buffer"));
                }
                // Only one monitor is supported. Keep old descriptors alive only until configure.
                self.surfaces.clear();
                self.current = None;
                self.surfaces.insert(
                    surface_id,
                    Surface {
                        id: surface_id,
                        width,
                        height,
                        fourcc: drm_fourcc,
                        stride,
                        modifier,
                        kind,
                        buffers: files,
                        pixels: if kind == SurfaceKind::Shm {
                            vec![0; size as usize]
                        } else {
                            Vec::new()
                        },
                        initialized: false,
                    },
                );
            }
            Message::FrameReady {
                surface_id,
                buffer,
                seq,
                flags,
                damage,
                ..
            } => {
                let surface = self
                    .surfaces
                    .get_mut(&surface_id)
                    .ok_or_else(|| invalid("frame has no surface"))?;
                let file = surface
                    .buffers
                    .get(buffer as usize)
                    .ok_or_else(|| invalid("frame buffer is out of range"))?;
                for fd in &packet.fds {
                    wait_fence(fd.as_raw_fd())?;
                }
                let mut damage: Vec<_> = damage
                    .into_iter()
                    .filter_map(|r| clip(r, surface.width, surface.height))
                    .collect();
                if !surface.initialized {
                    damage = vec![Rect {
                        x: 0,
                        y: 0,
                        width: surface.width,
                        height: surface.height,
                    }];
                }
                if surface.kind == SurfaceKind::Shm {
                    for rect in &damage {
                        for y in rect.y..rect.y + rect.height {
                            let offset =
                                u64::from(y) * u64::from(surface.stride) + u64::from(rect.x) * 4;
                            let row = &mut surface.pixels
                                [offset as usize..offset as usize + rect.width as usize * 4];
                            file.read_exact_at(row, offset)?;
                        }
                    }
                }
                surface.initialized = true;
                let frame = Frame {
                    surface: surface_id,
                    buffer,
                    seq,
                    flags,
                    damage,
                };
                self.current = Some(frame.clone());
                return Ok(Some(frame));
            }
            Message::CursorShape {
                width,
                height,
                hot_x,
                hot_y,
                argb,
            } => {
                self.cursor.width = width;
                self.cursor.height = height;
                self.cursor.hot_x = hot_x;
                self.cursor.hot_y = hot_y;
                self.cursor.argb = argb;
                self.cursor.generation += 1;
            }
            Message::CursorState { x, y, visible } => {
                self.cursor.x = x;
                self.cursor.y = y;
                self.cursor.visible = visible;
            }
            Message::LedState { mask } => self.leds = mask & 7,
            _ => return Err(invalid("unexpected server display message")),
        }
        Ok(None)
    }

    pub fn release(&self, frame: &Frame) -> io::Result<()> {
        self.client.release(frame.surface, frame.buffer, frame.seq)
    }

    pub fn send(&self, messages: impl IntoIterator<Item = Message>) -> io::Result<()> {
        if self.client.capabilities.has(Capabilities::INPUT) {
            for message in messages {
                self.client.send(&message)?;
            }
        }
        Ok(())
    }

    pub fn resize(&mut self, now: Instant, width: u32, height: u32) {
        if width > 0 && height > 0 {
            self.resize = Some((now, width.min(16384), height.min(16384)));
        }
    }

    pub fn tick(&mut self, now: Instant) -> io::Result<()> {
        if let Some((when, width, height)) = self.resize
            && now.saturating_duration_since(when) >= Duration::from_millis(200)
        {
            self.send([Message::RequestSize { width, height }])?;
            self.resize = None;
        }
        Ok(())
    }
}

pub fn clip(rect: Rect, width: u32, height: u32) -> Option<Rect> {
    let right = rect.x.saturating_add(rect.width).min(width);
    let bottom = rect.y.saturating_add(rect.height).min(height);
    (right > rect.x && bottom > rect.y).then(|| Rect {
        x: rect.x,
        y: rect.y,
        width: right - rect.x,
        height: bottom - rect.y,
    })
}

pub fn wait_fence(fd: i32) -> io::Result<()> {
    let mut pollfd = libc::pollfd {
        fd,
        events: libc::POLLIN,
        revents: 0,
    };
    loop {
        // SAFETY: pollfd points to one live pollfd; the borrowed descriptor stays open.
        let result = unsafe { libc::poll(&mut pollfd, 1, 5000) };
        if result < 0 && io::Error::last_os_error().kind() == io::ErrorKind::Interrupted {
            continue;
        }
        if result < 0 {
            return Err(io::Error::last_os_error());
        }
        if result == 0 {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "display acquire fence timed out",
            ));
        }
        if pollfd.revents & (libc::POLLERR | libc::POLLNVAL) != 0 {
            return Err(invalid("display acquire fence failed"));
        }
        return Ok(());
    }
}
