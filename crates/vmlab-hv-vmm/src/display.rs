//! Standard VGA scanout and platform input adapters for the display server.

use std::io;
use std::os::unix::net::UnixListener;
use std::path::Path;
use std::sync::{Arc, Weak, atomic::Ordering};

use parking_lot::Mutex;
use vmlab_hv_display::{DisplayServer, Frame, FrameSource, InputSink, Message, Pixels, Rect};

use crate::{error::VmmError, vm::VmShared};

#[derive(Clone)]
pub(crate) enum Video {
    Standard(Arc<Mutex<vmlab_hv_devices::display::StandardVga>>),
    Virtio(Arc<Mutex<vmlab_hv_devices::display::virtio_vga::VirtioVga>>),
}

impl Video {
    pub(crate) fn io_read(&self, port: u16, data: &mut [u8]) -> () {
        match self {
            Self::Standard(device) => device.lock().io_read(port, data),
            Self::Virtio(device) => device.lock().io_read(port, data),
        }
    }
    pub(crate) fn io_write(&self, port: u16, data: &[u8]) -> () {
        match self {
            Self::Standard(device) => device.lock().io_write(port, data),
            Self::Virtio(device) => device.lock().io_write(port, data),
        }
    }
    pub(crate) fn legacy_read(&self, offset: u64, data: &mut [u8]) -> () {
        match self {
            Self::Standard(device) => device.lock().legacy_read(offset, data),
            Self::Virtio(device) => device.lock().legacy_read(offset, data),
        }
    }
    pub(crate) fn legacy_write(&self, offset: u64, data: &[u8]) -> () {
        match self {
            Self::Standard(device) => device.lock().legacy_write(offset, data),
            Self::Virtio(device) => device.lock().legacy_write(offset, data),
        }
    }
    pub(crate) fn frame(&self) -> vmlab_hv_devices::display::Frame {
        match self {
            Self::Standard(device) => device.lock().frame(),
            Self::Virtio(device) => device.lock().frame(),
        }
    }
    pub(crate) fn linear_framebuffer(&self) -> vmlab_hv_devices::GuestMemory {
        match self {
            Self::Standard(device) => device.lock().linear_framebuffer(),
            Self::Virtio(device) => device.lock().linear_framebuffer(),
        }
    }
    pub(crate) fn mark_framebuffer_dirty(&self) -> () {
        match self {
            Self::Standard(device) => device.lock().mark_framebuffer_dirty(),
            Self::Virtio(device) => device.lock().mark_framebuffer_dirty(),
        }
    }
    pub(crate) fn framebuffer_size(&self) -> usize {
        match self {
            Self::Standard(_) => 16 << 20,
            Self::Virtio(_) => 8 << 20,
        }
    }
    pub(crate) fn pci_id(&self) -> u32 {
        match self {
            Self::Standard(_) => 0x11111234,
            Self::Virtio(_) => 0x10501af4,
        }
    }
    pub(crate) fn cursor(&self) -> vmlab_hv_display::Cursor {
        let Self::Virtio(device) = self else {
            return Default::default();
        };
        let c = device.lock().cursor();
        vmlab_hv_display::Cursor {
            width: c.width,
            height: c.height,
            hot_x: c.hot_x,
            hot_y: c.hot_y,
            argb: c.pixels,
            x: c.x.min(i32::MAX as u32) as i32,
            y: c.y.min(i32::MAX as u32) as i32,
            visible: c.visible,
        }
    }
    pub(crate) fn request_size(&self, width: u32, height: u32) {
        if let Self::Virtio(device) = self {
            let _ = device.lock().request_size(width, height);
        }
    }
}

struct Source(Weak<VmShared>);
impl FrameSource for Source {
    fn frame(&self) -> io::Result<Frame> {
        let shared = self
            .0
            .upgrade()
            .ok_or_else(|| io::Error::other("machine has exited"))?;
        let vga = shared
            .vga
            .lock()
            .clone()
            .ok_or_else(|| io::Error::other("machine has no VGA"))?;
        let dirty = shared.vga_framebuffer_dirty();
        let frame = {
            if dirty {
                vga.mark_framebuffer_dirty();
            }
            vga.frame()
        };
        Ok(Frame {
            generation: shared.display_generation.load(Ordering::SeqCst),
            width: frame.mode.width,
            height: frame.mode.height,
            stride: frame.mode.stride,
            drm_fourcc: vmlab_hv_display::protocol::XRGB8888,
            modifier: 0,
            timestamp_ns: crate::chipset::host_monotonic_ns(),
            damage: if frame.dirty {
                vec![Rect {
                    x: 0,
                    y: 0,
                    width: frame.mode.width,
                    height: frame.mode.height,
                }]
            } else {
                vec![]
            },
            pixels: Pixels::Shm(Arc::new(frame.pixels)),
        })
    }
    fn readback(&self, frame: &Frame) -> io::Result<Vec<u8>> {
        match &frame.pixels {
            Pixels::Shm(bytes) => Ok(bytes.as_ref().clone()),
            _ => Err(io::Error::other("VGA source only exports shm")),
        }
    }
}

struct Input {
    shared: Weak<VmShared>,
    pointer: Mutex<vmlab_hv_devices::usb::PointerEvent>,
}
impl InputSink for Input {
    fn input(&self, message: Message) {
        let Some(shared) = self.shared.upgrade() else {
            return;
        };
        match message {
            Message::Key { qcode, down } => {
                if let Some(chipset) = shared.chipset.lock().as_ref()
                    && let Err(e) = chipset.ps2.lock().inject_key(&qcode, down)
                {
                    eprintln!("display key: {e}");
                }
            }
            Message::RequestSize { width, height } => {
                if let Some(vga) = shared.vga.lock().as_ref() {
                    vga.request_size(width, height);
                }
            }
            message @ (Message::PointerAbs { .. }
            | Message::Buttons { .. }
            | Message::Scroll { .. }) => {
                let mut pointer = self.pointer.lock();
                pointer.wheel = 0;
                match message {
                    Message::PointerAbs {
                        x,
                        y,
                        width,
                        height,
                    } => {
                        pointer.x = ((x as u64 * 32767) / width.saturating_sub(1).max(1) as u64)
                            .min(32767) as u16;
                        pointer.y = ((y as u64 * 32767) / height.saturating_sub(1).max(1) as u64)
                            .min(32767) as u16;
                    }
                    Message::Buttons { mask } => pointer.buttons = (mask & 31) as u8,
                    Message::Scroll { dy, .. } => pointer.wheel = dy.clamp(-127, 127) as i8,
                    _ => unreachable!(),
                }
                if let Some(xhci) = shared.xhci.lock().as_ref()
                    && let Err(e) = xhci.lock().inject_pointer(*pointer)
                {
                    eprintln!("display pointer: {e}");
                }
            }
            _ => {}
        }
    }
}

pub(crate) fn serve(shared: &Arc<VmShared>, directory: &Path) -> Result<(), VmmError> {
    if shared.vga.lock().is_none() {
        return Ok(());
    }
    let native = directory.join("display.sock");
    let rfb = directory.join("vnc.sock");
    if native.exists() || rfb.exists() {
        return Err(VmmError::Control(
            "display.sock or vnc.sock already exists".into(),
        ));
    }
    let native_listener = UnixListener::bind(&native)
        .map_err(|e| VmmError::Control(format!("binding {}: {e}", native.display())))?;
    let rfb_listener = UnixListener::bind(&rfb)
        .map_err(|e| VmmError::Control(format!("binding {}: {e}", rfb.display())))?;
    let server = Arc::new(
        DisplayServer::new(
            Arc::new(Source(Arc::downgrade(shared))),
            Arc::new(Input {
                shared: Arc::downgrade(shared),
                pointer: Mutex::new(vmlab_hv_devices::usb::PointerEvent {
                    x: 0,
                    y: 0,
                    buttons: 0,
                    wheel: 0,
                }),
            }),
        )
        .map_err(|e| VmmError::Other(format!("creating display server: {e}")))?,
    );
    for (listener, rfb, name) in [
        (native_listener, false, "display-native"),
        (rfb_listener, true, "display-rfb"),
    ] {
        let server = server.clone();
        let clients = shared.display_clients.clone();
        std::thread::Builder::new()
            .name(name.into())
            .spawn(move || {
                for stream in listener.incoming() {
                    let stream = match stream {
                        Ok(stream) => stream,
                        Err(error) => {
                            eprintln!("{name}: {error}");
                            break;
                        }
                    };
                    let server = server.clone();
                    let clients = clients.clone();
                    clients.fetch_add(1, Ordering::Relaxed);
                    std::thread::spawn(move || {
                        let _ = if rfb {
                            server.serve_rfb(stream)
                        } else {
                            server.serve_native(stream)
                        };
                        clients.fetch_sub(1, Ordering::Relaxed);
                    });
                }
            })
            .map_err(|e| VmmError::Other(format!("spawning {name}: {e}")))?;
    }
    *shared.display.lock() = Some(server);
    Ok(())
}
