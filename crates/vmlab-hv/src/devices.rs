//! The real device wiring: builds the devices `vmlab-hv-devices` provides
//! from a machine description. One seam, one module; the VMM's builder does
//! the placement.

use parking_lot::Mutex;
use std::fs::OpenOptions;
use std::path::Path;
use std::sync::Arc;

use vmlab_disk::Format;
use vmlab_hv_devices::serial::{Serial, SerialOut};
use vmlab_hv_devices::virtio::block::Block;
use vmlab_hv_devices::virtio::console::{Console, PortConfig};
use vmlab_hv_devices::virtio::fs::{Fs, FsConfig};
use vmlab_hv_devices::virtio::net::{Net, NetConfig};
use vmlab_hv_devices::virtio::pci::VirtioPciDevice;
use vmlab_hv_devices::{GuestMemory, IrqLine, MsiSink, VirtioDevice};
use vmlab_hv_proto::machine::{Disk, DiskFormat, FsShare, Nic, NicAttachment, SerialPort};
use vmlab_hv_vmm::DeviceUnavailable;
use vmlab_hv_vmm::factory::{BuiltBlk, BuiltPci, BuiltSerial, SharedDisk, SharedImage};

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
    ) -> Result<BuiltPci, DeviceUnavailable> {
        let dev = Arc::new(Mutex::new(VirtioPciDevice::new(
            id,
            device,
            mem.clone(),
            Some(msi),
            None,
        )));
        Ok(BuiltPci {
            function: dev.clone(),
            snapshot: Some(dev),
        })
    }
}

impl vmlab_hv_vmm::DeviceFactory for DeviceSet {
    fn serial(
        &self,
        log_path: &Path,
        irq: Arc<dyn IrqLine>,
    ) -> Result<BuiltSerial, DeviceUnavailable> {
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
        let dev = Arc::new(Mutex::new(Serial::new(
            "serial0",
            SerialOut::new(Box::new(file)),
            irq,
        )));
        Ok(BuiltSerial {
            device: dev.clone(),
            snapshot: Some(dev),
        })
    }

    fn virtio_blk(
        &self,
        disk: &Disk,
        mem: &GuestMemory,
        msi: Arc<dyn MsiSink>,
    ) -> Result<BuiltBlk, DeviceUnavailable> {
        let format = match disk.format {
            DiskFormat::Raw => Format::Raw,
            DiskFormat::Qcow2 => Format::Qcow2,
        };
        // One open image shared by the device and the VMM's snapshot
        // orchestration (D14): the block device runs on a SharedDisk
        // forwarding to the same handle the registry keeps.
        let image: SharedImage = Arc::new(Mutex::new(
            vmlab_disk::open(&disk.path, format, disk.read_only).map_err(|e| {
                DeviceUnavailable::new(
                    "virtio-blk",
                    format!(
                        "cannot open disk {} (id `{}`): {e}",
                        disk.path.display(),
                        disk.id
                    ),
                )
            })?,
        ));
        let pci = self.wrap(
            disk.id.clone(),
            Box::new(Block::new(
                disk.id.clone(),
                Box::new(SharedDisk(image.clone())),
            )),
            mem,
            msi,
        )?;
        Ok(BuiltBlk { pci, disk: image })
    }

    fn virtio_console(
        &self,
        ports: &[SerialPort],
        mem: &GuestMemory,
        msi: Arc<dyn MsiSink>,
    ) -> Result<BuiltPci, DeviceUnavailable> {
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
    ) -> Result<BuiltPci, DeviceUnavailable> {
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
        mem: &GuestMemory,
        msi: Arc<dyn MsiSink>,
    ) -> Result<BuiltPci, DeviceUnavailable> {
        let id = format!("fs-{}", share.tag);
        let fs = Fs::new(
            id.clone(),
            FsConfig {
                tag: share.tag.clone(),
                socket: share.socket.clone(),
            },
        )
        .map_err(|e| {
            DeviceUnavailable::new(
                "vhost-user-fs",
                format!("share `{}`: {e}", share.tag),
            )
        })?;
        self.wrap(id, Box::new(fs), mem, msi)
    }
}

/// `aa:bb:cc:dd:ee:ff` to bytes.
fn parse_mac(mac: &str) -> Result<[u8; 6], DeviceUnavailable> {
    let bytes = mac
        .split(':')
        .map(|part| u8::from_str_radix(part, 16))
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| DeviceUnavailable::new("virtio-net", format!("bad MAC {mac:?}: {e}")))?;
    let bytes: [u8; 6] = bytes
        .try_into()
        .map_err(|_| DeviceUnavailable::new("virtio-net", format!("bad MAC {mac:?}")))?;
    Ok(bytes)
}
