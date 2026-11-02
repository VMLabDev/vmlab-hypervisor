//! Keyboard grabs complement winit's pointer confinement.

use anyhow::{Context, Result, ensure};
use raw_window_handle::{HasDisplayHandle, HasWindowHandle, RawDisplayHandle, RawWindowHandle};
use wayland_client::{
    Connection, Dispatch, Proxy, QueueHandle,
    protocol::{wl_registry, wl_seat, wl_surface},
};
use wayland_protocols::wp::keyboard_shortcuts_inhibit::zv1::client::{
    zwp_keyboard_shortcuts_inhibit_manager_v1 as manager,
    zwp_keyboard_shortcuts_inhibitor_v1 as inhibitor,
};
use winit::window::Window;
use x11_dl::xlib;

#[derive(Default)]
struct State {
    manager: Option<manager::ZwpKeyboardShortcutsInhibitManagerV1>,
    seats: Vec<wl_seat::WlSeat>,
}

impl Dispatch<wl_registry::WlRegistry, ()> for State {
    fn event(
        state: &mut Self,
        registry: &wl_registry::WlRegistry,
        event: wl_registry::Event,
        _: &(),
        _: &Connection,
        queue: &QueueHandle<Self>,
    ) {
        if let wl_registry::Event::Global {
            name,
            interface,
            version,
        } = event
        {
            match interface.as_str() {
                "zwp_keyboard_shortcuts_inhibit_manager_v1" => {
                    state.manager = Some(registry.bind(name, version.min(1), queue, ()))
                }
                "wl_seat" => state
                    .seats
                    .push(registry.bind(name, version.min(1), queue, ())),
                _ => {}
            }
        }
    }
}
wayland_client::delegate_noop!(State: ignore wl_seat::WlSeat);
wayland_client::delegate_noop!(State: ignore manager::ZwpKeyboardShortcutsInhibitManagerV1);
wayland_client::delegate_noop!(State: ignore inhibitor::ZwpKeyboardShortcutsInhibitorV1);

pub enum KeyboardGrab {
    X11 {
        api: Box<xlib::Xlib>,
        display: *mut xlib::Display,
    },
    Wayland {
        connection: Connection,
        inhibitors: Vec<inhibitor::ZwpKeyboardShortcutsInhibitorV1>,
        manager: manager::ZwpKeyboardShortcutsInhibitManagerV1,
        seats: Vec<wl_seat::WlSeat>,
    },
}

impl KeyboardGrab {
    pub fn new(window: &Window) -> Result<Self> {
        match (
            window.display_handle()?.as_raw(),
            window.window_handle()?.as_raw(),
        ) {
            (RawDisplayHandle::Xlib(display), RawWindowHandle::Xlib(window)) => {
                let display = display
                    .display
                    .context("Xlib display is missing")?
                    .as_ptr()
                    .cast();
                let api = Box::new(xlib::Xlib::open()?);
                // SAFETY: the display and window are borrowed from winit, used on
                // its event-loop thread, and the grab is dropped before the window.
                let status = unsafe {
                    (api.XGrabKeyboard)(
                        display,
                        window.window,
                        xlib::False,
                        xlib::GrabModeAsync,
                        xlib::GrabModeAsync,
                        xlib::CurrentTime,
                    )
                };
                ensure!(
                    status == xlib::GrabSuccess,
                    "X11 keyboard grab failed: {status}"
                );
                Ok(Self::X11 { api, display })
            }
            (RawDisplayHandle::Wayland(display), RawWindowHandle::Wayland(window)) => {
                // SAFETY: these are borrowed winit handles. The grab is destroyed before
                // the window, and the foreign backend never closes winit's display.
                let backend = unsafe {
                    wayland_backend::client::Backend::from_foreign_display(
                        display.display.as_ptr().cast(),
                    )
                };
                let connection = Connection::from_backend(backend);
                let mut queue = connection.new_event_queue::<State>();
                let _registry = connection.display().get_registry(&queue.handle(), ());
                let mut state = State::default();
                queue.roundtrip(&mut state)?;
                let manager = state
                    .manager
                    .context("compositor does not support keyboard shortcuts inhibition")?;
                ensure!(!state.seats.is_empty(), "compositor has no input seat");
                // SAFETY: the surface is a live borrowed wl_surface on this connection.
                let id = unsafe {
                    wayland_backend::client::ObjectId::from_ptr(
                        wl_surface::WlSurface::interface(),
                        window.surface.as_ptr().cast(),
                    )
                }?;
                let surface = wl_surface::WlSurface::from_id(&connection, id)?;
                let inhibitors = state
                    .seats
                    .iter()
                    .map(|seat| manager.inhibit_shortcuts(&surface, seat, &queue.handle(), ()))
                    .collect();
                connection.flush()?;
                Ok(Self::Wayland {
                    connection,
                    inhibitors,
                    manager,
                    seats: state.seats,
                })
            }
            _ => anyhow::bail!("keyboard grab needs Linux X11 or Wayland"),
        }
    }
}

impl Drop for KeyboardGrab {
    fn drop(&mut self) {
        match self {
            Self::X11 { api, display } => {
                // SAFETY: App drops this grab before its borrowed winit display.
                unsafe {
                    (api.XUngrabKeyboard)(*display, xlib::CurrentTime);
                    (api.XFlush)(*display);
                }
            }
            Self::Wayland {
                connection,
                inhibitors,
                manager,
                seats,
            } => {
                for inhibitor in inhibitors {
                    inhibitor.destroy();
                }
                manager.destroy();
                // wl_seat version 1 has no release request. Closing our event queue
                // leaves these borrowed-connection proxies for display teardown.
                seats.clear();
                let _ = connection.flush();
            }
        }
    }
}
