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
    pub(crate) fn accelerated(&self) -> bool {
        matches!(self, Self::Virtio(device) if device.lock().accelerated())
    }
    fn dma_frame(&self) -> io::Result<Option<vmlab_hv_devices::virtio::gpu::DmaFrame>> {
        match self {
            Self::Standard(_) => Ok(None),
            Self::Virtio(device) => device.lock().dma_frame().map_err(io::Error::other),
        }
    }
    pub(crate) fn set_aperture_base(&self, base: Option<u64>) -> Result<(), VmmError> {
        if let Self::Virtio(device) = self {
            device
                .lock()
                .set_aperture_base(base)
                .map_err(|e| VmmError::Other(e.to_string()))?;
        }
        Ok(())
    }
    pub(crate) fn io_read(&self, port: u16, data: &mut [u8]) {
        match self {
            Self::Standard(device) => device.lock().io_read(port, data),
            Self::Virtio(device) => device.lock().io_read(port, data),
        }
    }
    pub(crate) fn io_write(&self, port: u16, data: &[u8]) {
        match self {
            Self::Standard(device) => device.lock().io_write(port, data),
            Self::Virtio(device) => device.lock().io_write(port, data),
        }
    }
    pub(crate) fn legacy_read(&self, offset: u64, data: &mut [u8]) {
        match self {
            Self::Standard(device) => device.lock().legacy_read(offset, data),
            Self::Virtio(device) => device.lock().legacy_read(offset, data),
        }
    }
    pub(crate) fn legacy_write(&self, offset: u64, data: &[u8]) {
        match self {
            Self::Standard(device) => device.lock().legacy_write(offset, data),
            Self::Virtio(device) => device.lock().legacy_write(offset, data),
        }
    }
    pub(crate) fn frame(&self) -> (u64, vmlab_hv_devices::display::Frame) {
        match self {
            Self::Standard(device) => (0, device.lock().frame()),
            Self::Virtio(device) => {
                let mut device = device.lock();
                (device.scanout_generation(), device.frame())
            }
        }
    }
    pub(crate) fn linear_framebuffer(&self) -> vmlab_hv_devices::GuestMemory {
        match self {
            Self::Standard(device) => device.lock().linear_framebuffer(),
            Self::Virtio(device) => device.lock().linear_framebuffer(),
        }
    }
    pub(crate) fn mark_framebuffer_dirty(&self) {
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

struct Source {
    shared: Weak<VmShared>,
    generation: Mutex<((u64, u64), u64)>,
    readbacks: Mutex<Vec<Readback>>,
}
struct Readback {
    fd: Weak<std::os::fd::OwnedFd>,
    pixels: Arc<Vec<u8>>,
}
impl FrameSource for Source {
    fn frame(&self) -> io::Result<Frame> {
        self.readbacks
            .lock()
            .retain(|entry| entry.fd.strong_count() != 0);
        let shared = self
            .shared
            .upgrade()
            .ok_or_else(|| io::Error::other("machine has exited"))?;
        let vga = shared
            .vga
            .lock()
            .clone()
            .ok_or_else(|| io::Error::other("machine has no VGA"))?;
        if let Some(dma) = vga.dma_frame()? {
            let mut readbacks = self.readbacks.lock();
            readbacks.push(Readback {
                fd: Arc::downgrade(&dma.fd),
                pixels: dma.pixels,
            });
            let generation = match &vga {
                Video::Virtio(device) => device.lock().scanout_generation(),
                _ => 0,
            };
            return Ok(Frame {
                generation: generation
                    .wrapping_add(shared.display_generation.load(Ordering::SeqCst)),
                width: dma.width,
                height: dma.height,
                stride: dma.stride,
                drm_fourcc: dma.fourcc,
                modifier: dma.modifier,
                timestamp_ns: crate::chipset::host_monotonic_ns(),
                damage: vec![Rect {
                    x: 0,
                    y: 0,
                    width: dma.width,
                    height: dma.height,
                }],
                pixels: Pixels::Dmabuf {
                    fd: dma.fd,
                    acquire: dma.acquire,
                },
            });
        }
        let dirty = shared.vga_framebuffer_dirty();
        let (scanout_generation, frame) = {
            if dirty {
                vga.mark_framebuffer_dirty();
            }
            vga.frame()
        };
        let generation = {
            let mut state = self.generation.lock();
            let key = (
                shared.display_generation.load(Ordering::SeqCst),
                scanout_generation,
            );
            if state.0 != key {
                state.0 = key;
                state.1 = state.1.wrapping_add(1);
            }
            state.1
        };
        Ok(Frame {
            generation,
            width: frame.mode.width,
            height: frame.mode.height,
            stride: frame.mode.stride,
            drm_fourcc: vmlab_hv_display::protocol::XRGB8888,
            modifier: 0,
            timestamp_ns: crate::chipset::host_monotonic_ns(),
            damage: frame
                .damage
                .into_iter()
                .map(|r| Rect {
                    x: r.x,
                    y: r.y,
                    width: r.width,
                    height: r.height,
                })
                .collect(),
            pixels: Pixels::Shm(Arc::new(frame.pixels)),
        })
    }
    fn readback(&self, frame: &Frame) -> io::Result<Vec<u8>> {
        match &frame.pixels {
            Pixels::Shm(bytes) => Ok(bytes.as_ref().clone()),
            Pixels::Dmabuf { fd, .. } => self
                .readbacks
                .lock()
                .iter()
                .find(|entry| entry.fd.ptr_eq(&Arc::downgrade(fd)))
                .map(|entry| entry.pixels.as_ref().clone())
                .ok_or_else(|| io::Error::other("GPU readback no longer available")),
        }
    }
}

struct Input {
    shared: Weak<VmShared>,
    pointer: Mutex<vmlab_hv_devices::usb::PointerEvent>,
}

fn pointer_message(pointer: &mut vmlab_hv_devices::usb::PointerEvent, message: Message) {
    pointer.wheel = 0;
    match message {
        Message::PointerAbs {
            x,
            y,
            width,
            height,
        } => {
            pointer.x =
                ((x as u64 * 32767) / width.saturating_sub(1).max(1) as u64).min(32767) as u16;
            pointer.y =
                ((y as u64 * 32767) / height.saturating_sub(1).max(1) as u64).min(32767) as u16;
        }
        Message::Buttons { mask } => {
            pointer.buttons = ((mask & 0x19) | ((mask & 2) << 1) | ((mask & 4) >> 1)) as u8
        }
        Message::Scroll { dy, .. } => pointer.wheel = -dy.clamp(-127, 127) as i8,
        _ => unreachable!(),
    }
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
                pointer_message(&mut pointer, message);
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

pub(crate) fn serve(shared: &Arc<VmShared>, native: &Path, rfb: &Path) -> Result<(), VmmError> {
    if shared.vga.lock().is_none() {
        return Ok(());
    }
    if native == rfb {
        return Err(VmmError::Control(
            "native and RFB sockets must be different".into(),
        ));
    }
    if native.exists() || rfb.exists() {
        return Err(VmmError::Control(
            "display.sock or vnc.sock already exists".into(),
        ));
    }
    let native_listener = UnixListener::bind(native)
        .map_err(|e| VmmError::Control(format!("binding {}: {e}", native.display())))?;
    let rfb_listener = match UnixListener::bind(rfb) {
        Ok(listener) => listener,
        Err(error) => {
            drop(native_listener);
            std::fs::remove_file(native).map_err(|cleanup| {
                VmmError::Control(format!(
                    "binding {}: {error}; removing newly bound {}: {cleanup}",
                    rfb.display(),
                    native.display()
                ))
            })?;
            return Err(VmmError::Control(format!(
                "binding {}: {error}",
                rfb.display()
            )));
        }
    };
    let server = Arc::new(
        DisplayServer::new(
            Arc::new(Source {
                shared: Arc::downgrade(shared),
                generation: Mutex::new(((0, 0), 0)),
                readbacks: Mutex::new(Vec::new()),
            }),
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
        listener
            .set_nonblocking(true)
            .map_err(|e| VmmError::Control(format!("configuring {name}: {e}")))?;
        let server = server.clone();
        let weak = Arc::downgrade(shared);
        std::thread::Builder::new()
            .name(name.into())
            .spawn(move || {
                let mut workers = Vec::new();
                while weak.upgrade().is_some_and(|shared| !shared.exited()) {
                    let stream = match listener.accept() {
                        Ok((stream, _)) => stream,
                        Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                            std::thread::sleep(std::time::Duration::from_millis(10));
                            continue;
                        }
                        Err(error) => {
                            eprintln!("{name}: {error}");
                            break;
                        }
                    };
                    let server = server.clone();
                    workers.retain(|worker: &std::thread::JoinHandle<()>| !worker.is_finished());
                    workers.push(std::thread::spawn(move || {
                        let _ = if rfb {
                            server.serve_rfb(stream)
                        } else {
                            server.serve_native(stream)
                        };
                    }));
                }
                server.shutdown();
                for worker in workers {
                    let _ = worker.join();
                }
            })
            .map_err(|e| VmmError::Other(format!("spawning {name}: {e}")))?;
    }
    *shared.display.lock() = Some(server);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn native_buttons_and_scroll_match_usb_hid_conventions() {
        let mut pointer = vmlab_hv_devices::usb::PointerEvent::default();
        pointer_message(&mut pointer, Message::Buttons { mask: 2 });
        assert_eq!(pointer.buttons, 4);
        pointer_message(&mut pointer, Message::Buttons { mask: 4 });
        assert_eq!(pointer.buttons, 2);
        pointer_message(&mut pointer, Message::Scroll { dx: 0, dy: 1 });
        assert_eq!(pointer.wheel, -1);
        pointer_message(
            &mut pointer,
            Message::Scroll {
                dx: 0,
                dy: i32::MIN,
            },
        );
        assert_eq!(pointer.wheel, 127);
        pointer_message(
            &mut pointer,
            Message::PointerAbs {
                x: 400,
                y: 300,
                width: 801,
                height: 601,
            },
        );
        assert_eq!((pointer.x, pointer.y, pointer.wheel), (16383, 16383, 0));
    }
}
