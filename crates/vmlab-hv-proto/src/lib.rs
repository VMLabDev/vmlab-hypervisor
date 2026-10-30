//! The contract between vmlab and `vmlab-hv`: the machine description passed at
//! launch, and the control protocol spoken on the control socket.
//!
//! The control protocol is JSON lines over a unix socket. A request carries an
//! id and a typed command; its reply carries the same id and a typed result or
//! error. Events are unsolicited lines without an id. The first exchange is a
//! version handshake; nothing else is accepted before it.
//!
//! [`codec`] frames the messages on any byte stream, [`server`] serves
//! connections on the hypervisor side, and [`client`] talks to a hypervisor
//! from vmlab or a test harness.

pub mod codec;
pub mod control;
pub mod machine;

pub use machine::MachineDescription;

/// Version of the control protocol. Both sides refuse a different version.
pub const PROTO_VERSION: u32 = 1;
