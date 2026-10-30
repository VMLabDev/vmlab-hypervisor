//! Device models for vmlab-hv.
//!
//! Devices are written against the traits in this file and know nothing about
//! KVM: the VMM routes guest port I/O and MMIO to [`BusDevice`]s, PCI
//! configuration to [`PciDevice`]s, and wires interrupts in through
//! [`IrqLine`] and [`MsiSink`]. Virtio devices implement [`VirtioDevice`] and
//! sit behind the virtio-pci transport. Every device that holds guest-visible
//! state implements [`Snapshot`].

use std::sync::Arc;

pub mod ahci;
pub mod audio;
pub mod chipset;
pub mod display;
pub mod hda;
pub mod pflash;
pub mod serial;
pub mod tpm;
pub mod usb;
pub mod virtio;

/// Guest physical memory as devices see it.
pub type GuestMemory = vm_memory::GuestMemoryMmap<()>;

/// A level-triggered legacy interrupt line (e.g. the UART's IRQ 4).
pub trait IrqLine: Send + Sync {
    fn set_level(&self, high: bool);
}

/// One MSI or MSI-X message.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MsiMessage {
    pub address: u64,
    pub data: u32,
}

/// Delivers MSI messages to the guest.
pub trait MsiSink: Send + Sync {
    fn signal(&self, msg: MsiMessage);
}

/// A device on the port-I/O or MMIO bus. `offset` is relative to the base the
/// device was registered at.
pub trait BusDevice: Send {
    fn read(&mut self, offset: u64, data: &mut [u8]);
    fn write(&mut self, offset: u64, data: &[u8]);
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BarKind {
    Mmio32,
    Mmio64 { prefetchable: bool },
    Io,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BarInfo {
    pub index: u8,
    pub size: u64,
    pub kind: BarKind,
}

/// A PCI function. The VMM's PCI root owns config-space routing and BAR
/// placement; the device owns the contents of its config space and BARs.
pub trait PciDevice: Send {
    /// Read a 32-bit config register (`reg` is the dword index).
    fn config_read(&mut self, reg: usize) -> u32;
    /// Write `data` at byte `offset` within dword `reg`.
    fn config_write(&mut self, reg: usize, offset: u64, data: &[u8]);
    fn bars(&self) -> Vec<BarInfo>;
    fn bar_read(&mut self, bar: u8, offset: u64, data: &mut [u8]);
    fn bar_write(&mut self, bar: u8, offset: u64, data: &[u8]);
}

/// Interrupt delivery a virtio transport gives its device.
pub trait VirtioInterrupt: Send + Sync {
    fn notify_queue(&self, queue: u16);
    fn notify_config(&self);
}

/// A virtio device behind a transport. Virtio devices hold guest-visible
/// state (queues, features), so they implement [`Snapshot`]; the transport
/// saves its own state and the VMM snapshots the device through it.
pub trait VirtioDevice: Snapshot + Send {
    /// Virtio device id (2 = block, 1 = net, 3 = console, 26 = fs).
    fn device_type(&self) -> u32;
    /// Maximum size of each queue; the length is the queue count.
    fn queue_max_sizes(&self) -> &[u16];
    fn features(&self) -> u64;
    fn ack_features(&mut self, features: u64);
    fn read_config(&self, offset: u64, data: &mut [u8]);
    fn write_config(&mut self, offset: u64, data: &[u8]);
    /// Bytes of device-specific configuration readable at offset 0. The
    /// virtio-pci transport reports this as the device-configuration
    /// capability length.
    fn config_len(&self) -> u64 {
        0
    }

    /// The driver set DRIVER_OK: start using the queues.
    fn activate(
        &mut self,
        mem: GuestMemory,
        interrupt: Arc<dyn VirtioInterrupt>,
        queues: Vec<virtio_queue::Queue>,
    ) -> Result<(), DeviceError>;
    /// The driver kicked queue `index`.
    fn queue_notify(&mut self, index: u16);
    /// The driver reset the device. Returns to the pre-activation state.
    fn reset(&mut self);
}

/// Guest-visible state a device saves into and restores from a snapshot.
/// `Send` because the VMM keeps snapshot handles in its shared state.
pub trait Snapshot: Send {
    /// Stable id of this device instance in the machine, used as the section key.
    fn state_id(&self) -> String;
    fn save_state(&self) -> Result<Vec<u8>, DeviceError>;
    fn restore_state(&mut self, state: &[u8]) -> Result<(), DeviceError>;
}

/// Layered snapshot access to a PCI function: the function's own state and
/// the state of the device it wraps, as separate sections. The VMM saves
/// and restores the function's state first: a virtio transport's restore
/// re-activates the wrapped device with the restored queues, so the wrapped
/// device's own state lands after it.
pub trait PciSnapshot: Send {
    /// Stable id of this function instance, keying its snapshot sections.
    fn snapshot_id(&self) -> String;
    fn save_function_state(&self) -> Result<Vec<u8>, DeviceError>;
    fn save_wrapped_state(&self) -> Result<Vec<u8>, DeviceError>;
    fn restore_function_state(&mut self, state: &[u8]) -> Result<(), DeviceError>;
    fn restore_wrapped_state(&mut self, state: &[u8]) -> Result<(), DeviceError>;
}

#[derive(Debug, thiserror::Error)]
pub enum DeviceError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("guest memory: {0}")]
    Memory(String),
    #[error("snapshot state: {0}")]
    State(String),
    #[error("{0}")]
    Other(String),
}
