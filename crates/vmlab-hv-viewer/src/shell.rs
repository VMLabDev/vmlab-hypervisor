//! winit event loop. Display reads and clipboard calls never run in its input handlers.

use crate::{
    Core, Options,
    clipboard::{LabClipboard, Worker},
    grab::KeyboardGrab,
    render::Renderer,
};
use anyhow::{Context, Result};
use std::net::Shutdown;
use std::sync::Arc;
use std::time::{Duration, Instant};
use vmlab_hv_display::{Client, Message, Packet, SurfaceKind};
use winit::{
    application::ApplicationHandler,
    event::{ElementState, MouseButton, MouseScrollDelta, WindowEvent},
    event_loop::{ActiveEventLoop, ControlFlow, EventLoop},
    platform::scancode::PhysicalKeyExtScancode,
    window::{CursorGrabMode, Window, WindowId},
};

enum Event {
    Display(std::io::Result<Packet>),
}

struct App {
    options: Options,
    core: Core,
    // Renderer, keyboard grab, and all borrowed display handles die before the window.
    renderer: Option<Renderer>,
    keyboard: Option<KeyboardGrab>,
    window: Option<Window>,
    clipboard: Option<Worker>,
    error: Option<anyhow::Error>,
    pointer: (f64, f64),
    wheel: f64,
    last_title: String,
}

impl App {
    fn fail(&mut self, event_loop: &ActiveEventLoop, result: Result<()>) {
        if let Err(error) = result {
            self.error = Some(error);
            event_loop.exit();
        }
    }

    fn title(&mut self) {
        let locks = [
            if self.core.leds & 2 != 0 { "Num" } else { "" },
            if self.core.leds & 4 != 0 { "Caps" } else { "" },
            if self.core.leds & 1 != 0 {
                "Scroll"
            } else {
                ""
            },
        ]
        .join(" ");
        let title = format!(
            "{} - {} - release {} - {}",
            self.options.machine.as_deref().unwrap_or("vmlab-hv"),
            if self.core.input.grabbed {
                "grabbed"
            } else {
                "click to grab"
            },
            self.options.release_chord,
            locks.trim()
        );
        if title != self.last_title {
            if let Some(window) = &self.window {
                window.set_title(&title);
            }
            self.last_title = title;
        }
    }

    fn ungrab(&mut self) -> Result<()> {
        let messages = self.core.input.release();
        self.core.send(messages)?;
        self.keyboard.take();
        if let Some(window) = &self.window {
            window.set_cursor_grab(CursorGrabMode::None)?;
        }
        self.title();
        Ok(())
    }

    fn grab(&mut self) -> Result<()> {
        let window = self.window.as_ref().context("window not ready")?;
        let keyboard = KeyboardGrab::new(window)?;
        window
            .set_cursor_grab(CursorGrabMode::Confined)
            .or_else(|_| window.set_cursor_grab(CursorGrabMode::Locked))?;
        self.keyboard = Some(keyboard);
        self.core.input.grab();
        self.title();
        Ok(())
    }

    fn display(&mut self, packet: Packet) -> Result<()> {
        let frame = self.core.receive(packet)?;
        if let (Some(window), Some(renderer)) = (&self.window, &mut self.renderer) {
            window.set_cursor_visible(!self.core.input.grabbed);
            if let Some(frame) = frame {
                let surface = self
                    .core
                    .surfaces
                    .get(&frame.surface)
                    .context("missing surface")?;
                let result =
                    renderer.present(window, Some(surface), Some(&frame), &self.core.cursor);
                // present waits for GPU completion before returning. Failed imports never submit.
                self.core.release(&frame)?;
                match result {
                    Ok(()) => {}
                    Err(error) if surface.kind == SurfaceKind::Dmabuf => {
                        eprintln!(
                            "dma-buf import/presentation failed; requesting shared memory: {error:#}"
                        );
                        self.core.client.cpu_copy(frame.surface)?;
                    }
                    Err(error) => return Err(error),
                }
            } else {
                window.request_redraw();
            }
        } else if let Some(frame) = frame {
            self.core.release(&frame)?;
        }
        self.title();
        Ok(())
    }

    fn window_event(&mut self, event: WindowEvent) -> Result<()> {
        match event {
            WindowEvent::Resized(size) => {
                self.core.resize(Instant::now(), size.width, size.height);
                if let Some(renderer) = &mut self.renderer {
                    renderer.resize();
                }
                if let Some(window) = &self.window {
                    window.request_redraw();
                }
            }
            WindowEvent::Focused(focused) => {
                if let Some(clipboard) = &self.clipboard {
                    clipboard.focus(focused);
                }
                if !focused {
                    self.ungrab()?;
                }
            }
            WindowEvent::KeyboardInput { event, .. } => {
                if let Some(code) = event.physical_key.to_scancode() {
                    let messages = self
                        .core
                        .input
                        .key(code, event.state == ElementState::Pressed);
                    self.core.send(messages)?;
                    if !self.core.input.grabbed {
                        self.ungrab()?;
                    }
                }
            }
            WindowEvent::MouseInput { state, button, .. } => {
                if state == ElementState::Pressed
                    && !self.core.input.grabbed
                    && let Err(error) = self.grab()
                {
                    eprintln!("input grab failed: {error:#}");
                    return Ok(());
                }
                let bit = match button {
                    MouseButton::Left => 1,
                    MouseButton::Right => 4,
                    MouseButton::Middle => 2,
                    MouseButton::Back => 8,
                    MouseButton::Forward => 16,
                    _ => 0,
                };
                let messages = self.core.input.button(bit, state == ElementState::Pressed);
                self.core.send(messages)?;
                self.send_pointer()?;
            }
            WindowEvent::CursorMoved { position, .. } => {
                self.pointer = (position.x, position.y);
                self.send_pointer()?;
            }
            WindowEvent::MouseWheel { delta, .. } if self.core.input.grabbed => {
                self.wheel += match delta {
                    MouseScrollDelta::LineDelta(_, y) => y as f64,
                    MouseScrollDelta::PixelDelta(p) => p.y / 40.,
                };
                let steps = self.wheel.trunc() as i32;
                self.wheel -= steps as f64;
                if steps != 0 {
                    self.core.send([Message::Scroll { dx: 0, dy: -steps }])?;
                }
            }
            WindowEvent::RedrawRequested => {
                if let (Some(window), Some(renderer)) = (&self.window, &mut self.renderer) {
                    renderer.present(window, None, None, &self.core.cursor)?;
                }
            }
            _ => {}
        }
        self.title();
        Ok(())
    }

    fn send_pointer(&self) -> Result<()> {
        if self.core.input.grabbed
            && let Some(frame) = &self.core.current
            && let Some(surface) = self.core.surfaces.get(&frame.surface)
            && let Some(window) = &self.window
        {
            let size = window.inner_size();
            self.core.send(crate::input::pointer(
                self.pointer,
                (size.width, size.height),
                (surface.width, surface.height),
            ))?;
        }
        Ok(())
    }
}

impl ApplicationHandler<Event> for App {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.window.is_some() {
            return;
        }
        let result = (|| -> Result<()> {
            let window = event_loop.create_window(
                Window::default_attributes()
                    .with_title("vmlab-hv-viewer")
                    .with_inner_size(winit::dpi::PhysicalSize::new(1024, 768)),
            )?;
            let renderer = Renderer::new(&window, !self.options.shm)?;
            if !renderer.dmabuf() && !self.options.shm {
                // The client initially requests dma-buf. CpuCopy is sent after the first
                // configure/frame when Vulkan cannot import; negotiation uses --shm on WSLg.
                eprintln!(
                    "Vulkan dma-buf extensions unavailable; using shared memory when configured"
                );
            }
            self.renderer = Some(renderer);
            self.window = Some(window);
            self.title();
            Ok(())
        })();
        self.fail(event_loop, result);
    }

    fn user_event(&mut self, event_loop: &ActiveEventLoop, event: Event) {
        let Event::Display(packet) = event;
        let result = packet
            .map_err(anyhow::Error::from)
            .and_then(|p| self.display(p));
        self.fail(event_loop, result);
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, _: WindowId, event: WindowEvent) {
        if matches!(event, WindowEvent::CloseRequested) {
            event_loop.exit();
            return;
        }
        let result = self.window_event(event);
        self.fail(event_loop, result);
    }

    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        let result = self.core.tick(Instant::now()).map_err(Into::into);
        self.fail(event_loop, result);
        event_loop.set_control_flow(ControlFlow::WaitUntil(
            Instant::now() + Duration::from_millis(10),
        ));
    }
}

struct Reader {
    client: Arc<Client>,
    thread: Option<std::thread::JoinHandle<()>>,
}
impl Drop for Reader {
    fn drop(&mut self) {
        let _ = self.client.stream().shutdown(Shutdown::Both);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

pub fn run(options: Options) -> Result<()> {
    let event_loop = EventLoop::<Event>::with_user_event().build()?;
    let core = Core::connect(&options.display, options.shm, &options.release_chord)?;
    let clipboard = match (&options.lab_socket, &options.machine) {
        (Some(socket), Some(machine)) => {
            Some(Worker::start(LabClipboard::new(socket, machine.clone()))?)
        }
        _ => None,
    };
    let client = core.client.clone();
    let source = client.clone();
    let proxy = event_loop.create_proxy();
    let thread = std::thread::Builder::new()
        .name("viewer-display".into())
        .spawn(move || {
            loop {
                let packet = source.receive();
                let failed = packet.is_err();
                if proxy.send_event(Event::Display(packet)).is_err() || failed {
                    break;
                }
            }
        })?;
    let _reader = Reader {
        client,
        thread: Some(thread),
    };
    let mut app = App {
        options,
        core,
        renderer: None,
        keyboard: None,
        window: None,
        clipboard,
        error: None,
        pointer: (0., 0.),
        wheel: 0.,
        last_title: String::new(),
    };
    let result = event_loop.run_app(&mut app);
    let _ = app.ungrab();
    result?;
    if let Some(error) = app.error.take() {
        return Err(error);
    }
    Ok(())
}
