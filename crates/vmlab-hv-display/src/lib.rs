//! Display transport shared by the hypervisor and its consumers.
//!
//! See `PROTOCOL.md` for the versioned wire contract and integration API.

mod client;
mod core;
pub mod protocol;
pub mod rfb;
mod transport;

pub use client::Client;
pub use core::{Cursor, DisplayServer, Frame, FrameSource, InputSink, Pixels};
pub use protocol::{Capabilities, FrameFlags, Message, Rect, SurfaceKind};
pub use transport::{Packet, receive, send};

use std::io;

pub(crate) fn invalid(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}
