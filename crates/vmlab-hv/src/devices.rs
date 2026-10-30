//! The real device wiring: builds the devices `vmlab-hv-devices` provides
//! from a machine description. One seam, one module; the VMM's builder does
//! the placement.
//!
//! Still missing here: the vhost-user-fs master (shared-memfd RAM is already
//! prepared by the VMM when a machine has shares; the device that hands it to
//! virtiofsd has not landed yet).

use parking_lot::Mutex;
use std::fs::OpenOptions;
use std::path::Path;
use std::sync::Arc;

use vmlab_disk::Format;
use vmlab_hv_devices::serial::{Serial, SerialOut};
use vmlab_hv_devices::virtio::block::Block;
use vmlab_hv_devices::virtio::console::{Console, PortConfig};
use vmlab_hv_devices::virtio::net::{Net, NetConfig};
use vmlab_hv_devices::virtio::pci::VirtioPciDevice;
use vmlab_hv_devices::{BusDevice, GuestMemory, IrqLine, MsiSink, PciDevice, VirtioDevice};
use vmlab_hv_proto::machine::{Disk, DiskFormat, FsShare, Nic, NicAttachment, SerialPort};

use vmlab_hv_vmm::error::DeviceUnavailable;

/// The device set the binary ships.
#[derive(Debug, Default)]
pub struct DeviceSet;

impl DeviceSet {
    fn wrap(
        &self,
        id: String,
        device: Box<dyn VirtioDevice>,
        mem: &GuestMemory,
        msi: Arc<dyn MsiSink>,
    ) -> Result<Arc<Mutex<dyn PciDevice>>, DeviceUnavailable> {
        Ok(Arc::new(Mutex::new(VirtioPciDevice::new(
            id,
            device,
            mem.clone(),
            Some(msi),
            None,
        ))))
    }
}

impl vmlab_hv_vmm::DeviceFactory for DeviceSet {
    fn serial(
        &self,
        log_path: &Path,
        irq: Arc<dyn IrqLine>,
    ) -> Result<Arc<Mutex<dyn BusDevice>>, DeviceUnavailable> {
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(log_path)
            .map_err(|e| {
                DeviceUnavailable::new(
                    "serial-16550a",
                    format!("cannot open serial log {}: {e}", log_path.display()),
                )
            })?;
        Ok(Arc::new(Mutex::new(Serial::new(
            "serial0",
            SerialOut::new(Box::new(file)),
            irq,
        ))))
    }

    fn virtio_blk(
        &self,
        disk: &Disk,
        mem: &GuestMemory,
        msi: Arc<dyn MsiSink>,
    ) -> Result<Arc<Mutex<dyn PciDevice>>, DeviceUnavailable> {
        let format = match disk.format {
            DiskFormat::Raw => Format::Raw,
            DiskFormat::Qcow2 => Format::Qcow2,
        };
        let image = vmlab_disk::open(&disk.path, format, disk.read_only).map_err(|e| {
            DeviceUnavailable::new(
                "virtio-blk",
                format!(
                    "cannot open disk {} (id `{}`): {e}",
                    disk.path.display(),
                    disk.id
                ),
            )
        })?;
        self.wrap(
            disk.id.clone(),
            Box::new(Block::new(disk.id.clone(), image)),
            mem,
            msi,
        )
    }

    fn virtio_console(
        &self,
        ports: &[SerialPort],
        mem: &GuestMemory,
        msi: Arc<dyn MsiSink>,
    ) -> Result<Arc<Mutex<dyn PciDevice>>, DeviceUnavailable> {
        let ports = ports
            .iter()
            .map(|p| PortConfig {
                name: p.name.clone(),
                path: p.socket.clone(),
            })
            .collect();
        let console = Console::new("console0", ports).map_err(|e| {
            DeviceUnavailable::new(
                "virtio-console",
                format!("cannot listen on port sockets: {e}"),
            )
        })?;
        self.wrap("console0".into(), Box::new(console), mem, msi)
    }

    fn virtio_net(
        &self,
        nic: &Nic,
        mem: &GuestMemory,
        msi: Arc<dyn MsiSink>,
    ) -> Result<Arc<Mutex<dyn PciDevice>>, DeviceUnavailable> {
        let mac = parse_mac(&nic.mac)?;
        let id = format!("nic-{}", nic.mac);
        let switch = match &nic.attachment {
            NicAttachment::Stream { socket } => socket.clone(),
        };
        let config = NetConfig {
            id: id.clone(),
            mac,
            mtu: nic.mtu.unwrap_or(1500),
            switch,
        };
        let net = Net::new(config)
            .map_err(|e| DeviceUnavailable::new("virtio-net", format!("cannot connect: {e}")))?;
        self.wrap(id, Box::new(net), mem, msi)
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
                "the vhost-user master for share `{}` has not landed yet",
                share.tag
            ),
        ))
    }
}

/// `aa:bb:cc:dd:ee:ff` to bytes.
fn parse_mac(mac: &str) -> Result<[u8; 6], DeviceUnavailable> {
    let mut bytes = [0u8; 6];
    let parts: Vec<_> = mac.split(':').collect();
    if parts.len() != 6 {
        return Err(DeviceUnavailable::new(
            "virtio-net",
            format!("malformed MAC address {mac:?}"),
        ));
    }
    for (slot, part) in bytes.iter_mut().zip(parts) {
        *slot = u8::from_str_radix(part, 16).map_err(|_| {
            DeviceUnavailable::new("virtio-net", format!("malformed MAC address {mac:?}"))
        })?;
    }
    Ok(bytes)
}
