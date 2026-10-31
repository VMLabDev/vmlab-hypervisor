use parking_lot::Mutex;
use std::{io, os::unix::net::UnixListener, path::Path, sync::Arc, thread::JoinHandle};
use vmlab_hv_devices::virtio::gpu::Gpu;
use vmlab_hv_display::{DisplayServer, Frame, FrameFlags, FrameSource, InputSink, Message, Pixels};

struct Source {
    gpu: Gpu,
    placeholder: Arc<Vec<u8>>,
}
impl FrameSource for Source {
    fn frame(&self) -> io::Result<Frame> {
        if let Some(frame) = self.gpu.frame() {
            Ok(Frame {
                flags: Default::default(),
                generation: self.gpu.scanout_generation(),
                width: frame.mode.width,
                height: frame.mode.height,
                stride: frame.mode.stride,
                drm_fourcc: vmlab_hv_display::protocol::XRGB8888,
                modifier: 0,
                timestamp_ns: crate::chipset::host_monotonic_ns(),
                damage: frame
                    .damage
                    .into_iter()
                    .map(|r| vmlab_hv_display::Rect {
                        x: r.x,
                        y: r.y,
                        width: r.width,
                        height: r.height,
                    })
                    .collect(),
                pixels: Pixels::Shm(Arc::new(frame.pixels)),
            })
        } else {
            Ok(Frame {
                flags: FrameFlags(FrameFlags::PLACEHOLDER),
                generation: self.gpu.scanout_generation(),
                width: 640,
                height: 480,
                stride: 2560,
                drm_fourcc: vmlab_hv_display::protocol::XRGB8888,
                modifier: 0,
                timestamp_ns: crate::chipset::host_monotonic_ns(),
                damage: Vec::new(),
                pixels: Pixels::Shm(self.placeholder.clone()),
            })
        }
    }
    fn readback(&self, frame: &Frame) -> io::Result<Vec<u8>> {
        match &frame.pixels {
            Pixels::Shm(bytes) => Ok((**bytes).clone()),
            _ => Err(io::Error::other(
                "2D virtio-gpu requires shared-memory frames",
            )),
        }
    }
}
struct Input {
    gpu: Gpu,
    xhci: Option<Arc<Mutex<vmlab_hv_devices::usb::XhciController>>>,
    pointer: Mutex<vmlab_hv_devices::usb::PointerEvent>,
}
impl InputSink for Input {
    fn input(&self, message: Message) {
        match message {
            Message::RequestSize { width, height } => {
                let _ = self.gpu.request_size(width, height);
            }
            message @ (Message::PointerAbs { .. }
            | Message::Buttons { .. }
            | Message::Scroll { .. }) => {
                let mut pointer = self.pointer.lock();
                crate::display::pointer_message(&mut pointer, message);
                if let Some(xhci) = &self.xhci
                    && let Err(e) = xhci.lock().inject_pointer(*pointer)
                {
                    eprintln!("RISC-V tablet: {e}");
                }
            }
            _ => {}
        }
    }
}

type DisplayWorkers = (Arc<DisplayServer>, Vec<JoinHandle<()>>);

pub(super) fn serve(
    shared: &Arc<super::runtime::Shared>,
    native: &Path,
    rfb: &Path,
) -> Result<DisplayWorkers, String> {
    if native == rfb || native.exists() || rfb.exists() {
        return Err("display sockets must be distinct unoccupied paths".into());
    }
    let gpu = shared
        .gpu
        .as_ref()
        .ok_or("virt display requires a GPU")?
        .clone();
    let native_listener = UnixListener::bind(native).map_err(|e| e.to_string())?;
    let rfb_listener = UnixListener::bind(rfb).map_err(|e| e.to_string())?;
    let server = Arc::new(
        DisplayServer::new(
            Arc::new(Source {
                gpu: gpu.clone(),
                placeholder: Arc::new(crate::display::placeholder_pixels()),
            }),
            Arc::new(Input {
                gpu,
                xhci: shared.xhci.clone(),
                pointer: Mutex::default(),
            }),
        )
        .map_err(|e| e.to_string())?,
    );
    let mut workers = Vec::new();
    for (listener, rfb) in [(native_listener, false), (rfb_listener, true)] {
        listener.set_nonblocking(true).map_err(|e| e.to_string())?;
        let server = server.clone();
        let weak = Arc::downgrade(shared);
        workers.push(std::thread::spawn(move || {
            let mut sessions = Vec::new();
            while weak.upgrade().is_some_and(|s| !s.exited()) {
                match listener.accept() {
                    Ok((stream, _)) => {
                        let server = server.clone();
                        sessions.push(std::thread::spawn(move || {
                            let _ = if rfb {
                                server.serve_rfb(stream)
                            } else {
                                server.serve_native(stream)
                            };
                        }));
                    }
                    Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                        std::thread::sleep(std::time::Duration::from_millis(10))
                    }
                    Err(e) => {
                        eprintln!("RISC-V display accept: {e}");
                        break;
                    }
                }
            }
            server.shutdown();
            for session in sessions {
                let _ = session.join();
            }
        }));
    }
    let publisher = server.clone();
    let weak = Arc::downgrade(shared);
    workers.push(std::thread::spawn(move || {
        while let Some(shared) = weak.upgrade() {
            if shared.exited() {
                break;
            }
            let result = publisher.publish().and_then(|()| {
                let c = shared.gpu.as_ref().unwrap().cursor();
                publisher.set_cursor(vmlab_hv_display::Cursor {
                    width: c.width,
                    height: c.height,
                    hot_x: c.hot_x,
                    hot_y: c.hot_y,
                    argb: c.pixels,
                    x: c.x as i32,
                    y: c.y as i32,
                    visible: c.visible,
                })
            });
            if let Err(error) = result {
                shared.exit(crate::vm::ExitReason::Error(format!("display: {error}")));
                break;
            }
            drop(shared);
            std::thread::sleep(std::time::Duration::from_millis(16));
        }
    }));
    Ok((server, workers))
}
