//! Q35's ICH9 and ISA devices, independent of the VMM.
//!
//! Timers consume running guest time through `advance(ns)`. The machine must
//! advance them before register accesses and from its timer event loop, and
//! stop advancing while paused. IRQ handles and callbacks remain attached
//! across snapshot restore; only guest-visible state is serialized.

pub mod cpu;
pub mod hpet;
pub mod isa;
pub mod lpc;
pub mod ps2;
pub mod rtc;
pub mod smbus;
