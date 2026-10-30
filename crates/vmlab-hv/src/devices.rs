//! The real device wiring: builds the devices `vmlab-hv-devices` provides
//! from a machine description. One seam, one module; the VMM's builder does
//! the placement.

use parking_lot::Mutex;
use std::fs::OpenOptions;
use std::path::Path;
use std::sync::Arc;

use vmlab_hv_devices::serial::{Serial, SerialOut};
use vmlab_hv_devices::virtio::block::Block;
use vmlab_hv_devices::virtio::console::{Console, PortConfig};
use vmlab_hv_devices::virtio::fs::{Fs, FsConfig};
use vmlab_hv_devices::virtio::net::{Net, NetConfig};
use vmlab_hv_devices::virtio::pci::VirtioPciDevice;
use vmlab_hv_devices::{GuestMemory, IrqLine, VirtioDevice};
use vmlab_hv_proto::machine::{Disk, FsShare, Nic, NicAttachment, SerialPort};
use vmlab_hv_vmm::DeviceUnavailable;
use vmlab_hv_vmm::factory::{BuiltBlk, BuiltPci, BuiltSerial, SharedDisk};

/// The device set the binary ships.
#[derive(Debug, Default)]
pub struct DeviceSet;

impl DeviceSet {
    fn wrap(
        &self,
        id: String,
        device: Box<dyn VirtioDevice>,
        mem: &GuestMemory,
        msi: vmlab_hv_vmm::factory::PciWiring,
        rom: Option<&Path>,
    ) -> Result<BuiltPci, DeviceUnavailable> {
        let transport = if msi.transitional {
            VirtioPciDevice::new_transitional(
                id,
                device,
                mem.clone(),
                Some(msi.msi),
                msi.intx,
                msi.msix_vectors,
            )
            .map_err(|e| DeviceUnavailable::new("virtio-pci", e.to_string()))?
        } else {
            VirtioPciDevice::new(id, device, mem.clone(), Some(msi.msi), msi.intx)
        };
        let transport = if let Some(path) = rom {
            let image = std::fs::read(path).map_err(|e| {
                DeviceUnavailable::new("virtio-net ROM", format!("{}: {e}", path.display()))
            })?;
            transport
                .with_option_rom(&image)
                .map_err(|e| DeviceUnavailable::new("virtio-net ROM", e.to_string()))?
        } else {
            transport
        };
        let dev = Arc::new(Mutex::new(transport));
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
        msi: vmlab_hv_vmm::factory::PciWiring,
    ) -> Result<BuiltBlk, DeviceUnavailable> {
        // One open image shared by the device and the VMM's snapshot
        // orchestration (D14): the block device runs on a SharedDisk
        // forwarding to the same handle the registry keeps.
        let image = vmlab_hv_vmm::factory::open_image(disk)?;
        let block = Block::new(disk.id.clone(), Box::new(SharedDisk(image.clone())));
        let block = if msi.transitional {
            block
                .with_num_queues(msi.msix_vectors.saturating_sub(1))
                .map_err(|e| DeviceUnavailable::new("virtio-blk", e.to_string()))?
        } else {
            block
        };
        let pci = self.wrap(disk.id.clone(), Box::new(block), mem, msi, None)?;
        Ok(BuiltBlk { pci, disk: image })
    }

    fn virtio_console(
        &self,
        ports: &[SerialPort],
        mem: &GuestMemory,
        msi: vmlab_hv_vmm::factory::PciWiring,
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
        self.wrap("console0".into(), Box::new(console), mem, msi, None)
    }

    fn virtio_net(
        &self,
        nic: &Nic,
        mem: &GuestMemory,
        msi: vmlab_hv_vmm::factory::PciWiring,
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
        self.wrap(id, Box::new(net), mem, msi, nic.rom.as_deref())
    }

    fn vhost_user_fs(
        &self,
        share: &FsShare,
        mem: &GuestMemory,
        msi: vmlab_hv_vmm::factory::PciWiring,
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
            DeviceUnavailable::new("vhost-user-fs", format!("share `{}`: {e}", share.tag))
        })?;
        self.wrap(id, Box::new(fs), mem, msi, None)
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

#[cfg(test)]
mod tests {
    use super::*;
    use vmlab_hv_vmm::DeviceFactory;

    struct Msi;
    impl vmlab_hv_devices::MsiSink for Msi {
        fn signal(&self, _: vmlab_hv_devices::MsiMessage) {}
    }

    #[test]
    fn block_multiqueue_policy_preserves_container_configuration() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("disk.raw");
        std::fs::File::create(&path)
            .unwrap()
            .set_len(65536)
            .unwrap();
        let disk: Disk = serde_json::from_value(serde_json::json!({
            "id":"test", "path":path, "format":"raw", "read_only":false
        }))
        .unwrap();
        let mem = GuestMemory::new();
        for (transitional, expected) in [(false, 0u16), (true, 2)] {
            let built = DeviceSet
                .virtio_blk(
                    &disk,
                    &mem,
                    vmlab_hv_vmm::factory::PciWiring {
                        msi: Arc::new(Msi),
                        intx: None,
                        transitional,
                        msix_vectors: 3,
                    },
                )
                .unwrap();
            let mut count = [0; 2];
            built.pci.function.lock().bar_read(
                4,
                if transitional { 0x2022 } else { 0x3022 },
                &mut count,
            );
            assert_eq!(u16::from_le_bytes(count), expected);
        }
    }
}
