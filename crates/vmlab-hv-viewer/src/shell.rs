//! winit event loop. Display reads and clipboard calls never run in its input handlers.

use crate::{
    Core, Options,
    clipboard::{LabClipboard, Worker},
    connection::{Connection, Event},
    grab::KeyboardGrab,
    render::Renderer,
};
use anyhow::{Context, Result};
use std::net::Shutdown;
use std::time::{Duration, Instant};
use vmlab_hv_display::{Message, Packet, SurfaceKind};
use winit::{
    application::ApplicationHandler,
    event::{ElementState, MouseButton, MouseScrollDelta, WindowEvent},
    event_loop::{ActiveEventLoop, ControlFlow, EventLoop},
    platform::scancode::PhysicalKeyExtScancode,
    window::{Window, WindowId},
};

struct App {
    options: Options,
    core: Option<Core>,
    // Renderer, keyboard grab, and all borrowed display handles die before the window.
    renderer: Option<Renderer>,
    keyboard: Option<KeyboardGrab>,
    window: Option<Window>,
    clipboard: Option<Worker>,
    error: Option<anyhow::Error>,
    pointer: (f64, f64),
    pointer_inside: bool,
    fallback_cursor: crate::Cursor,
    wheel: f64,
    last_title: String,
    shown_frame: bool,
    generation: u64,
    pending_releases: Vec<Message>,
}

impl App {
    fn fail(&mut self, event_loop: &ActiveEventLoop, result: Result<()>) {
        if let Err(error) = result {
            if self.window.is_some() && error.chain().any(|error| error.is::<std::io::Error>()) {
                if self.core.is_some() {
                    eprintln!("display connection failed; waiting to reconnect: {error:#}");
                }
                self.disconnected();
                return;
            }
            self.error = Some(error);
            event_loop.exit();
        }
    }

    fn disconnected(&mut self) {
        let Some(mut core) = self.core.take() else {
            return;
        };
        let _ = core.client.stream().shutdown(Shutdown::Both);
        self.pending_releases.extend(core.input.focus_lost());
        self.keyboard.take();
        self.wheel = 0.;
        if let Some(clipboard) = &self.clipboard {
            clipboard.focus(false);
        }
        if let Some(renderer) = &mut self.renderer {
            renderer.reset_display();
        }
        self.update_cursor();
        self.title();
        if let Some(window) = &self.window {
            window.request_redraw();
        }
    }

    fn title(&mut self) {
        let leds = self.core.as_ref().map_or(0, |core| core.leds);
        let locks = [
            if leds & 2 != 0 { "Num" } else { "" },
            if leds & 4 != 0 { "Caps" } else { "" },
            if leds & 1 != 0 { "Scroll" } else { "" },
        ]
        .join(" ");
        let title = format!(
            "{} - {} - release {} - {}",
            self.options.machine.as_deref().unwrap_or("vmlab-hv"),
            if !self.shown_frame {
                "waiting for display"
            } else if self.core.as_ref().is_some_and(|core| core.input.grabbed) {
                "keyboard grabbed"
            } else {
                "click to grab keyboard"
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
        let messages = self
            .core
            .as_mut()
            .map(|core| core.input.release())
            .unwrap_or_default();
        self.keyboard.take();
        self.update_cursor();
        self.title();
        if let Some(core) = &self.core {
            core.send(messages)?;
        }
        Ok(())
    }

    fn grab(&mut self) -> Result<()> {
        if self.core.as_ref().is_none_or(|core| !core.input.can_grab()) {
            return Ok(());
        }
        let window = self.window.as_ref().context("window not ready")?;
        let keyboard = KeyboardGrab::new(window)?;
        self.keyboard = Some(keyboard);
        self.core
            .as_mut()
            .context("display not connected")?
            .input
            .grab();
        self.update_cursor();
        self.title();
        Ok(())
    }

    fn update_cursor(&mut self) {
        let _ = self.position_fallback();
        if let Some(window) = &self.window {
            window.set_cursor_visible(
                self.core
                    .as_ref()
                    .is_none_or(|core| core.host_cursor_visible(self.pointer_inside)),
            );
        }
    }

    fn display(&mut self, packet: Packet) -> Result<()> {
        let frame = self
            .core
            .as_mut()
            .context("display not connected")?
            .receive(packet)?;
        self.update_cursor();
        let core = self.core.as_ref().context("display not connected")?;
        if let (Some(window), Some(renderer)) = (&self.window, &mut self.renderer) {
            if let Some(frame) = frame {
                let surface = core
                    .surfaces
                    .get(&frame.surface)
                    .context("missing surface")?;
                let cursor = if core.use_fallback_cursor() {
                    &self.fallback_cursor
                } else {
                    &core.cursor
                };
                let result = renderer.present(window, Some(surface), Some(&frame), cursor);
                // present waits for GPU completion before returning. Failed imports never submit.
                core.release(&frame)?;
                match result {
                    Ok(()) => self.shown_frame = true,
                    Err(error) if surface.kind == SurfaceKind::Dmabuf => {
                        eprintln!(
                            "dma-buf import/presentation failed; requesting shared memory: {error:#}"
                        );
                        core.client.cpu_copy(frame.surface)?;
                    }
                    Err(error) => return Err(error),
                }
            } else {
                window.request_redraw();
            }
        } else if let Some(frame) = frame {
            core.release(&frame)?;
        }
        self.title();
        Ok(())
    }

    fn window_event(&mut self, event: WindowEvent) -> Result<()> {
        if self.core.is_none()
            && matches!(
                event,
                WindowEvent::KeyboardInput { .. }
                    | WindowEvent::MouseInput { .. }
                    | WindowEvent::MouseWheel { .. }
            )
        {
            return Ok(());
        }
        match event {
            WindowEvent::Resized(size) => {
                if let Some(core) = &mut self.core {
                    core.resize(Instant::now(), size.width, size.height);
                }
                if let Some(renderer) = &mut self.renderer {
                    renderer.resize();
                }
                self.update_cursor();
                if let Some(window) = &self.window {
                    window.request_redraw();
                }
            }
            WindowEvent::Focused(focused) => {
                if let Some(clipboard) = &self.clipboard {
                    clipboard.focus(focused && self.core.is_some());
                }
                if !focused {
                    let messages = self
                        .core
                        .as_mut()
                        .map(|core| core.input.focus_lost())
                        .unwrap_or_default();
                    self.ungrab()?;
                    if let Some(core) = &self.core {
                        core.send(messages)?;
                    }
                }
            }
            WindowEvent::Occluded(occluded) => {
                if let Some(renderer) = &mut self.renderer {
                    renderer.occluded(occluded);
                }
                if !occluded && let Some(window) = &self.window {
                    window.request_redraw();
                }
            }
            WindowEvent::KeyboardInput { event, .. } => {
                if let Some(code) = event.physical_key.to_scancode() {
                    let core = self.core.as_mut().context("display not connected")?;
                    let was_grabbed = core.input.grabbed;
                    let messages = core.input.key(code, event.state == ElementState::Pressed);
                    if was_grabbed && !core.input.grabbed {
                        self.ungrab()?;
                    }
                    self.core
                        .as_ref()
                        .context("display not connected")?
                        .send(messages)?;
                }
            }
            WindowEvent::MouseInput { state, button, .. } if self.pointer_inside => {
                if state == ElementState::Pressed
                    && button == MouseButton::Left
                    && self.pointer_inside
                    && self.core.as_ref().is_some_and(|core| !core.input.grabbed)
                    && let Err(error) = self.grab()
                {
                    eprintln!("input grab failed: {error:#}");
                }
                let bit = match button {
                    MouseButton::Left => 1,
                    MouseButton::Right => 4,
                    MouseButton::Middle => 2,
                    MouseButton::Back => 8,
                    MouseButton::Forward => 16,
                    _ => 0,
                };
                self.send_pointer()?;
                let core = self.core.as_mut().context("display not connected")?;
                let messages = core.input.button(bit, state == ElementState::Pressed);
                core.send(messages)?;
            }
            WindowEvent::CursorMoved { position, .. } => {
                self.pointer = (position.x, position.y);
                self.send_pointer()?;
            }
            WindowEvent::CursorEntered { .. } => {
                self.pointer_inside = true;
                self.fallback_cursor.visible = true;
                self.update_cursor();
                if let Some(window) = &self.window {
                    window.request_redraw();
                }
            }
            WindowEvent::CursorLeft { .. } => {
                self.pointer_inside = false;
                self.fallback_cursor.visible = false;
                self.ungrab()?;
                if let Some(window) = &self.window {
                    window.request_redraw();
                }
            }
            WindowEvent::MouseWheel { delta, .. } if self.pointer_inside => {
                self.wheel += match delta {
                    MouseScrollDelta::LineDelta(_, y) => y as f64,
                    MouseScrollDelta::PixelDelta(p) => p.y / 40.,
                };
                let steps = self.wheel.trunc() as i32;
                self.wheel -= steps as f64;
                if steps != 0 {
                    self.core
                        .as_ref()
                        .context("display not connected")?
                        .send([Message::Scroll { dx: 0, dy: -steps }])?;
                }
            }
            WindowEvent::RedrawRequested => {
                if let (Some(window), Some(renderer)) = (&self.window, &mut self.renderer) {
                    let empty = crate::Cursor::default();
                    let cursor = if self
                        .core
                        .as_ref()
                        .is_some_and(|core| core.use_fallback_cursor())
                    {
                        &self.fallback_cursor
                    } else {
                        self.core.as_ref().map_or(&empty, |core| &core.cursor)
                    };
                    renderer.present(window, None, None, cursor)?;
                }
            }
            _ => {}
        }
        self.title();
        Ok(())
    }

    fn position_fallback(&mut self) -> Option<Message> {
        let core = self.core.as_ref()?;
        let frame = core.current.as_ref()?;
        let surface = core.surfaces.get(&frame.surface)?;
        let size = self.window.as_ref()?.inner_size();
        let message = crate::input::pointer(
            self.pointer,
            (size.width, size.height),
            (surface.width, surface.height),
        )?;
        if let Message::PointerAbs { x, y, .. } = &message {
            self.fallback_cursor.x = *x as i32;
            self.fallback_cursor.y = *y as i32;
        }
        Some(message)
    }

    fn send_pointer(&mut self) -> Result<()> {
        if self.pointer_inside
            && let Some(message) = self.position_fallback()
        {
            if self
                .core
                .as_ref()
                .is_some_and(|core| core.use_fallback_cursor())
                && let Some(window) = &self.window
            {
                window.request_redraw();
            }
            self.core
                .as_ref()
                .context("display not connected")?
                .send([message])?;
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
        let result = match event {
            Event::Packet(generation, packet)
                if generation == self.generation && self.core.is_some() =>
            {
                self.display(packet)
            }
            Event::Connected(generation, client) => (|| -> Result<()> {
                if let Some(renderer) = &mut self.renderer {
                    renderer.reset_display();
                }
                self.core = Some(Core::new(client, &self.options.release_chord)?);
                let core = self.core.as_mut().context("display not connected")?;
                self.generation = generation;
                core.send(self.pending_releases.iter().cloned())?;
                self.pending_releases.clear();
                if let Some(window) = &self.window {
                    let size = window.inner_size();
                    core.resize(Instant::now(), size.width, size.height);
                    if let Some(clipboard) = &self.clipboard {
                        clipboard.focus(window.has_focus());
                    }
                }
                self.update_cursor();
                self.title();
                Ok(())
            })(),
            Event::Disconnected(generation, error) if generation == self.generation => {
                if self.core.is_some() {
                    eprintln!("display disconnected; waiting to reconnect: {error}");
                }
                self.disconnected();
                Ok(())
            }
            Event::Gone => {
                event_loop.exit();
                Ok(())
            }
            Event::Failed(error) => {
                self.error = Some(error.into());
                event_loop.exit();
                Ok(())
            }
            _ => Ok(()),
        };
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
        if let Some(core) = &mut self.core {
            let result = core.tick(Instant::now()).map_err(Into::into);
            self.fail(event_loop, result);
        }
        event_loop.set_control_flow(ControlFlow::WaitUntil(
            Instant::now() + Duration::from_millis(10),
        ));
    }
}

pub fn run(options: Options) -> Result<()> {
    let event_loop = EventLoop::<Event>::with_user_event().build()?;
    let clipboard = match (&options.lab_socket, &options.machine) {
        (Some(socket), Some(machine)) => {
            Some(Worker::start(LabClipboard::new(socket, machine.clone()))?)
        }
        _ => None,
    };
    let proxy = event_loop.create_proxy();
    let _connection = Connection::start(options.display.clone(), options.shm, move |event| {
        proxy.send_event(event).is_ok()
    })?;
    let mut app = App {
        options,
        core: None,
        renderer: None,
        keyboard: None,
        window: None,
        clipboard,
        error: None,
        pointer: (0., 0.),
        pointer_inside: false,
        fallback_cursor: crate::Cursor {
            visible: false,
            ..crate::Cursor::fallback()
        },
        wheel: 0.,
        last_title: String::new(),
        shown_frame: false,
        generation: 0,
        pending_releases: Vec::new(),
    };
    let result = event_loop.run_app(&mut app);
    let _ = app.ungrab();
    result?;
    if let Some(error) = app.error.take() {
        return Err(error);
    }
    Ok(())
}
