//! xHCI 1.0 controller and USB HID tablet, implemented from Intel xHCI 1.2b
//! and USB HID 1.11. PCI layout and tablet descriptors are black-box QEMU
//! 11.1.2 observations. No QEMU or crosvm implementation code is included.
//!
//! The integration contract and black-box measurements are in
//! `usb/README.md` beside this module.

mod host;
mod ring;
mod state;
mod tablet;
mod xhci;
pub use host::HostUsbDevice;

pub use tablet::{PointerEvent, TABLET_REPORT_DESCRIPTOR, Tablet};
pub use xhci::{BAR_SIZE, TABLET_PORT, XhciController};

use crate::{DeviceError, Snapshot};

/// USB speed IDs used by the xHCI Slot Context and PORTSC registers.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum UsbSpeed {
    Full = 1,
    Low = 2,
    High = 3,
    Super = 4,
}

/// USB SETUP packet, with all multibyte fields in host byte order.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SetupPacket {
    pub request_type: u8,
    pub request: u8,
    pub value: u16,
    pub index: u16,
    pub length: u16,
}

impl SetupPacket {
    pub fn from_bytes(b: [u8; 8]) -> Self {
        Self {
            request_type: b[0],
            request: b[1],
            value: u16::from_le_bytes([b[2], b[3]]),
            index: u16::from_le_bytes([b[4], b[5]]),
            length: u16::from_le_bytes([b[6], b[7]]),
        }
    }
}

/// A NAK keeps the transfer queued. The VMM calls `XhciController::poll`
/// when an asynchronous backend becomes ready. IN data is returned in `Ack`;
/// OUT data is passed to the backend and acknowledged with an empty vector.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum UsbTransfer {
    Ack(Vec<u8>),
    Nak,
    Stall,
}

/// Port backend seam for future host USB passthrough. Endpoint addresses
/// have the USB direction bit, not the xHCI endpoint-context index.
/// Implementations must not block. Physical backends can refuse snapshots
/// through `Snapshot`; the controller propagates that refusal.
pub trait UsbDevice: Snapshot {
    fn speed(&self) -> UsbSpeed;
    fn reset(&mut self);
    fn control(&mut self, setup: SetupPacket, data: &[u8]) -> UsbTransfer;
    fn transfer(&mut self, endpoint: u8, data: &[u8], max_length: usize) -> UsbTransfer;
    /// Isochronous transfer: as `transfer`, plus the endpoint's packet
    /// size, which a physical backend needs to lay out the URB's packet
    /// descriptors. The default delegates to `transfer`.
    fn transfer_iso(
        &mut self,
        endpoint: u8,
        data: &[u8],
        max_length: usize,
        packet_size: u32,
    ) -> UsbTransfer {
        let _ = packet_size;
        self.transfer(endpoint, data, max_length)
    }
    /// The guest dropped this endpoint's transfer state (Stop Endpoint,
    /// Set TR Dequeue Pointer, Reset Device). Backends with in-flight work
    /// discard it, so a stale completion is never attributed to a new TD.
    fn endpoint_dropped(&mut self, endpoint: u8) {
        let _ = endpoint;
    }
    /// Whether the physical device went away. The controller reports lost
    /// ports so the machine can detach them.
    fn is_lost(&self) -> bool {
        false
    }
    /// Whether this backend passes a host device through; such machines
    /// refuse online snapshots (D9).
    fn is_host_backed(&self) -> bool {
        false
    }
    fn inject_pointer(&mut self, _event: PointerEvent) -> Result<(), DeviceError> {
        Err(DeviceError::Other(
            "USB device does not accept pointer input".into(),
        ))
    }
}

#[cfg(test)]
mod tests;
