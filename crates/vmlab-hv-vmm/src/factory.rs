//! The seam to the device models in `vmlab-hv-devices`.
//!
//! The devices crate is built in parallel with this one; the machine builder
//! asks a [`DeviceFactory`] for each device the machine description names.
//! The factory the binary passes decides what exists today: until a device
//! model lands, its constructor returns [`DeviceUnavailable`] and `build`
//! fails with a clear error naming the machine part that needed it — never a
//! panic.
//!
//! Constructors expected from `vmlab-hv-devices` when its work packages
//! land:
//!
//! - `serial::Serial16550A::new(log: File, irq: Arc<dyn IrqLine>)` for COM1;
//! - `virtio::VirtioPciDevice::new(id, Box<dyn VirtioDevice>, GuestMemory,
//!   msi, intx)` wrapping `block::Block`, `console::Console` and `net::Net`;
//! - a vhost-user-fs PCI device taking the share and the shared memory
//!   (still to come: the vhost-user master).

use std::path::Path;
use std::sync::Arc;
use parking_lot::Mutex;

use vmlab_hv_devices::{BusDevice, GuestMemory, IrqLine, MsiSink, PciDevice};
use vmlab_hv_proto::machine::{Disk, FsShare, Nic, SerialPort};

use crate::error::DeviceUnavailable;

/// Produces the device models a machine description names.
pub trait DeviceFactory: Send + Sync {
    /// The 16550A UART on COM1, appending output to the serial log.
    fn serial(
        &self,
        log_path: &Path,
        irq: Arc<dyn IrqLine>,
    ) -> Result<Arc<Mutex<dyn BusDevice>>, DeviceUnavailable>;

    /// A virtio-blk PCI function for one disk.
    fn virtio_blk(
        &self,
        disk: &Disk,
        mem: &GuestMemory,
        msi: Arc<dyn MsiSink>,
    ) -> Result<Arc<Mutex<dyn PciDevice>>, DeviceUnavailable>;

    /// A single multiport virtio-console PCI function with `ports` in order.
    fn virtio_console(
        &self,
        ports: &[SerialPort],
        mem: &GuestMemory,
        msi: Arc<dyn MsiSink>,
    ) -> Result<Arc<Mutex<dyn PciDevice>>, DeviceUnavailable>;

    /// A virtio-net PCI function for one NIC.
    fn virtio_net(
        &self,
        nic: &Nic,
        mem: &GuestMemory,
        msi: Arc<dyn MsiSink>,
    ) -> Result<Arc<Mutex<dyn PciDevice>>, DeviceUnavailable>;

    /// A vhost-user-fs PCI function backed by an external virtiofsd.
    fn vhost_user_fs(
        &self,
        share: &FsShare,
        mem: &GuestMemory,
        msi: Arc<dyn MsiSink>,
    ) -> Result<Arc<Mutex<dyn PciDevice>>, DeviceUnavailable>;
}

/// A factory with no devices: every constructor explains that the model has
/// not landed in `vmlab-hv-devices` yet. Used by the binary until the device
/// work packages land; tests supply their own.
#[derive(Debug, Default)]
pub struct NoDevices;

impl DeviceFactory for NoDevices {
    fn serial(
        &self,
        _log_path: &Path,
        _irq: Arc<dyn IrqLine>,
    ) -> Result<Arc<Mutex<dyn BusDevice>>, DeviceUnavailable> {
        Err(DeviceUnavailable::new(
            "serial-16550a",
            "the device model has not landed in vmlab-hv-devices yet",
        ))
    }

    fn virtio_blk(
        &self,
        disk: &Disk,
        _mem: &GuestMemory,
        _msi: Arc<dyn MsiSink>,
    ) -> Result<Arc<Mutex<dyn PciDevice>>, DeviceUnavailable> {
        Err(DeviceUnavailable::new(
            "virtio-blk",
            format!(
                "the device model has not landed in vmlab-hv-devices yet (disk `{}`)",
                disk.id
            ),
        ))
    }

    fn virtio_console(
        &self,
        ports: &[SerialPort],
        _mem: &GuestMemory,
        _msi: Arc<dyn MsiSink>,
    ) -> Result<Arc<Mutex<dyn PciDevice>>, DeviceUnavailable> {
        Err(DeviceUnavailable::new(
            "virtio-console",
            format!(
                "the device model has not landed in vmlab-hv-devices yet (ports {})",
                ports.iter().map(|p| p.name.as_str()).collect::<Vec<_>>().join(", ")
            ),
        ))
    }

    fn virtio_net(
        &self,
        nic: &Nic,
        _mem: &GuestMemory,
        _msi: Arc<dyn MsiSink>,
    ) -> Result<Arc<Mutex<dyn PciDevice>>, DeviceUnavailable> {
        Err(DeviceUnavailable::new(
            "virtio-net",
            format!(
                "the device model has not landed in vmlab-hv-devices yet (nic {})",
                nic.mac
            ),
        ))
    }

    fn vhost_user_fs(
        &self,
        share: &FsShare,
        _mem: &GuestMemory,
        _msi: Arc<dyn MsiSink>,
    ) -> Result<Arc<Mutex<dyn PciDevice>>, DeviceUnavailable> {
        Err(DeviceUnavailable::new(
            "vhost-user-fs",
            format!(
                "the device model has not landed in vmlab-hv-devices yet (share `{}`)",
                share.tag
            ),
        ))
    }
}
