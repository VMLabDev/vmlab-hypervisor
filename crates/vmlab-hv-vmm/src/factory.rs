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

use parking_lot::Mutex;
use std::path::Path;
use std::sync::Arc;

use vmlab_disk::DiskImage;
use vmlab_hv_devices::{
    BusDevice, GuestMemory, IrqLine, MsiSink, PciDevice, PciSnapshot, Snapshot,
};
use vmlab_hv_proto::machine::{Disk, FsShare, Nic, SerialPort};

use crate::error::DeviceUnavailable;

/// One open disk image shared between a block device and the VMM's snapshot
/// orchestration.
pub type SharedImage = Arc<Mutex<Box<dyn DiskImage>>>;

pub fn open_image(disk: &Disk) -> Result<SharedImage, DeviceUnavailable> {
    let path = disk.path.as_ref().ok_or_else(|| {
        DeviceUnavailable::new("disk", format!("disk `{}` requires an image path", disk.id))
    })?;
    let format = match disk.format {
        vmlab_hv_proto::machine::DiskFormat::Raw => vmlab_disk::Format::Raw,
        vmlab_hv_proto::machine::DiskFormat::Qcow2 => vmlab_disk::Format::Qcow2,
    };
    vmlab_disk::open(path, format, disk.read_only)
        .map(|image| Arc::new(Mutex::new(image)))
        .map_err(|e| {
            DeviceUnavailable::new(
                "disk",
                format!(
                    "cannot open disk {} (id `{}`): {e}",
                    path.display(),
                    disk.id
                ),
            )
        })
}

/// A built serial console: the bus device plus its snapshot handle (the
/// same object), when the model carries guest-visible state.
pub struct BuiltSerial {
    pub device: Arc<Mutex<dyn BusDevice>>,
    pub snapshot: Option<Arc<Mutex<dyn Snapshot>>>,
}

/// A built PCI function: the function the PCI root places, plus snapshot
/// access to the same object when the model carries guest-visible state.
pub struct BuiltPci {
    pub function: Arc<Mutex<dyn PciDevice>>,
    pub snapshot: Option<Arc<Mutex<dyn PciSnapshot>>>,
}

/// A built virtio-blk function plus the shared disk image handle: the block
/// device runs on this image and so does snapshot orchestration, through
/// one open file (D14).
pub struct BuiltBlk {
    pub pci: BuiltPci,
    pub disk: SharedImage,
}

/// Per-function transport policy and interrupt wiring, selected by the machine.
pub struct PciWiring {
    pub msi: Arc<dyn MsiSink>,
    pub intx: Option<Arc<dyn IrqLine>>,
    pub transitional: bool,
    pub msix_vectors: u16,
}

/// Produces the device models a machine description names.
pub trait DeviceFactory: Send + Sync {
    /// The 16550A UART on COM1, appending output to the serial log.
    fn serial(
        &self,
        log_path: &Path,
        irq: Arc<dyn IrqLine>,
    ) -> Result<BuiltSerial, DeviceUnavailable>;

    /// A virtio-blk PCI function for one disk.
    fn virtio_blk(
        &self,
        disk: &Disk,
        mem: &GuestMemory,
        msi: PciWiring,
    ) -> Result<BuiltBlk, DeviceUnavailable>;

    /// A single multiport virtio-console PCI function with `ports` in order.
    fn virtio_console(
        &self,
        ports: &[SerialPort],
        mem: &GuestMemory,
        msi: PciWiring,
    ) -> Result<BuiltPci, DeviceUnavailable>;

    /// A virtio-net PCI function for one NIC.
    fn virtio_net(
        &self,
        nic: &Nic,
        mem: &GuestMemory,
        msi: PciWiring,
    ) -> Result<BuiltPci, DeviceUnavailable>;

    /// A virtio-fs PCI function for one share: the external virtiofsd
    /// named by `socket`, or the in-process device rooted at `path`.
    fn fs_share(
        &self,
        share: &FsShare,
        mem: &GuestMemory,
        msi: PciWiring,
    ) -> Result<BuiltPci, DeviceUnavailable>;
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
    ) -> Result<BuiltSerial, DeviceUnavailable> {
        Err(DeviceUnavailable::new(
            "serial-16550a",
            "the device model has not landed in vmlab-hv-devices yet",
        ))
    }

    fn virtio_blk(
        &self,
        disk: &Disk,
        _mem: &GuestMemory,
        _msi: crate::factory::PciWiring,
    ) -> Result<BuiltBlk, DeviceUnavailable> {
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
        _msi: crate::factory::PciWiring,
    ) -> Result<BuiltPci, DeviceUnavailable> {
        Err(DeviceUnavailable::new(
            "virtio-console",
            format!(
                "the device model has not landed in vmlab-hv-devices yet (ports {})",
                ports
                    .iter()
                    .map(|p| p.name.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        ))
    }

    fn virtio_net(
        &self,
        nic: &Nic,
        _mem: &GuestMemory,
        _msi: crate::factory::PciWiring,
    ) -> Result<BuiltPci, DeviceUnavailable> {
        Err(DeviceUnavailable::new(
            "virtio-net",
            format!(
                "the device model has not landed in vmlab-hv-devices yet (nic {})",
                nic.mac
            ),
        ))
    }

    fn fs_share(
        &self,
        share: &FsShare,
        _mem: &GuestMemory,
        _msi: crate::factory::PciWiring,
    ) -> Result<BuiltPci, DeviceUnavailable> {
        Err(DeviceUnavailable::new(
            "virtio-fs",
            format!(
                "the device model has not landed in vmlab-hv-devices yet (share `{}`)",
                share.tag
            ),
        ))
    }
}

/// Forwards [`DiskImage`] to one shared handle, so a block device and the
/// VMM's snapshot orchestration use the same open image: qcow2 metadata
/// caches cannot be duplicated across two handles of one file.
///
/// The VMM holds the [`SharedImage`] in its snapshot registry; the block
/// device gets this box around a clone of it.
pub struct SharedDisk(pub SharedImage);

impl DiskImage for SharedDisk {
    fn size(&self) -> u64 {
        self.0.lock().size()
    }

    fn read_only(&self) -> bool {
        self.0.lock().read_only()
    }

    fn read_at(&mut self, offset: u64, buf: &mut [u8]) -> vmlab_disk::Result<()> {
        self.0.lock().read_at(offset, buf)
    }

    fn write_at(&mut self, offset: u64, buf: &[u8]) -> vmlab_disk::Result<()> {
        self.0.lock().write_at(offset, buf)
    }

    fn flush(&mut self) -> vmlab_disk::Result<()> {
        self.0.lock().flush()
    }

    fn discard(&mut self, offset: u64, len: u64) -> vmlab_disk::Result<()> {
        self.0.lock().discard(offset, len)
    }

    fn snapshots(&self) -> vmlab_disk::Result<Vec<vmlab_disk::SnapshotInfo>> {
        self.0.lock().snapshots()
    }

    fn snapshot_create(
        &mut self,
        name: &str,
        vm_state_size: u64,
        vm_clock_nsec: u64,
    ) -> vmlab_disk::Result<()> {
        self.0
            .lock()
            .snapshot_create(name, vm_state_size, vm_clock_nsec)
    }

    fn snapshot_apply(&mut self, name: &str) -> vmlab_disk::Result<vmlab_disk::SnapshotInfo> {
        self.0.lock().snapshot_apply(name)
    }

    fn snapshot_delete(&mut self, name: &str) -> vmlab_disk::Result<()> {
        self.0.lock().snapshot_delete(name)
    }

    fn write_vmstate(&mut self, offset: u64, buf: &[u8]) -> vmlab_disk::Result<()> {
        self.0.lock().write_vmstate(offset, buf)
    }

    fn read_vmstate(&mut self, offset: u64, buf: &mut [u8]) -> vmlab_disk::Result<()> {
        self.0.lock().read_vmstate(offset, buf)
    }
}
