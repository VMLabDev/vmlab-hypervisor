//! Standard VGA scanout and platform input adapters for the display server.

use std::io;
use std::os::unix::net::UnixListener;
use std::path::Path;
use std::sync::{Arc, Weak, atomic::Ordering};

use parking_lot::Mutex;
use vmlab_hv_display::{DisplayServer, Frame, FrameSource, InputSink, Message, Pixels, Rect};

use crate::{error::VmmError, vm::VmShared};

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
        let frame = vga.lock().frame();
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
        std::thread::Builder::new()
            .name(name.into())
            .spawn(move || {
                if let Err(e) = server.serve(listener, rfb) {
                    eprintln!("{name}: {e}");
                }
            })
            .map_err(|e| VmmError::Other(format!("spawning {name}: {e}")))?;
    }
    *shared.display.lock() = Some(server);
    Ok(())
}
