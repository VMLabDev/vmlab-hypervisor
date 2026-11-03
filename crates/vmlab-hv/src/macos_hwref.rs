//! P2 static PCI inventory from the shipping device constructors.

use parking_lot::Mutex;
use std::{collections::BTreeMap, sync::Arc};
use vmlab_hv_devices::{BarKind, GuestMemory, IrqLine, MsiMessage, MsiSink, PciDevice};
use vmlab_hv_proto::machine::{Nic, NicAttachment, SerialPort, SerialTransport};
use vmlab_hv_vmm::{
    DeviceFactory,
    factory::PciWiring,
    q35::{Mch, Pam},
};

struct Irq;
impl IrqLine for Irq {
    fn set_level(&self, _: bool) {}
}
impl MsiSink for Irq {
    fn signal(&self, _: MsiMessage) {}
}

#[test]
fn macos_p2_all_pci_identities_classes_and_bar_sizes_match_hwref() {
    let directory = tempfile::tempdir().unwrap();
    let network = directory.path().join("net.sock");
    let _switch = vmlab_hv_tests::switch::MiniSwitch::bind(&network).unwrap();
    let memory = GuestMemory::new();
    let irq = Arc::new(Irq);
    let wiring = |vectors| PciWiring {
        msi: irq.clone(),
        intx: Some(irq.clone()),
        transitional: true,
        msix_vectors: vectors,
    };
    let mut devices: BTreeMap<(u8, u8), Arc<Mutex<dyn PciDevice>>> = BTreeMap::new();
    let pam = Arc::new(Mutex::new(
        Pam::new(&vec![0xff; 256 << 10], None, 0).unwrap(),
    ));
    devices.insert((0, 0), Arc::new(Mutex::new(Mch::new(pam))));
    let console = super::devices::DeviceSet::default()
        .virtio_console(
            &[SerialPort {
                name: "vmlab.agent.0".into(),
                socket: directory.path().join("agent.sock"),
                transport: SerialTransport::Virtio,
            }],
            &memory,
            wiring(3),
        )
        .unwrap();
    devices.insert((1, 0), console.function);
    let nic = super::devices::DeviceSet::default()
        .virtio_net(
            &Nic {
                model: Default::default(),
                rom: None,
                pci: None,
                mac: "52:54:00:12:34:56".into(),
                mtu: Some(1500),
                attachment: NicAttachment::Stream { socket: network },
            },
            &memory,
            wiring(4),
        )
        .unwrap();
    devices.insert((2, 0), nic.function);
    let mut vga = vmlab_hv_devices::display::StandardVga::new("vga", &[0x55, 0xaa, 1]).unwrap();
    vga.enable_vmware();
    devices.insert((3, 0), Arc::new(Mutex::new(vga)));
    devices.insert(
        (4, 0),
        Arc::new(Mutex::new(vmlab_hv_devices::usb::XhciController::new(
            "xhci",
            memory.clone(),
            irq.clone(),
            Some(irq.clone()),
        ))),
    );
    devices.insert(
        (31, 0),
        Arc::new(Mutex::new(vmlab_hv_devices::chipset::lpc::Lpc::new(
            irq.clone(),
            Arc::new(|_| {}),
        ))),
    );
    devices.insert(
        (31, 2),
        Arc::new(Mutex::new(vmlab_hv_devices::ahci::Ahci::new(
            "ahci",
            memory,
            irq.clone(),
            irq.clone(),
        ))),
    );
    devices.insert(
        (31, 3),
        Arc::new(Mutex::new(vmlab_hv_devices::chipset::smbus::Smbus::new(
            "smbus", irq,
        ))),
    );
    let mut root = vmlab_hv_vmm::pci::PciRoot::q35(32 << 30);
    for (&(slot, function), device) in &devices {
        root.add_device_at(slot, function, device.clone()).unwrap();
    }
    let reference: Vec<serde_json::Value> = serde_json::from_str(include_str!(
        "../../../docs/hwref/q35-macos-tahoe/query-pci.json"
    ))
    .unwrap();
    let expected = reference[0]["devices"].as_array().unwrap();
    assert_eq!(expected.len(), devices.len());
    for record in expected {
        let address = (
            record["slot"].as_u64().unwrap() as u8,
            record["function"].as_u64().unwrap() as u8,
        );
        let mut device = devices[&address].lock();
        let id = record["id"]["vendor"].as_u64().unwrap()
            | (record["id"]["device"].as_u64().unwrap() << 16);
        assert_eq!(u64::from(device.config_read(0)), id, "{address:?} identity");
        assert_eq!(
            u64::from(device.config_read(2) >> 16),
            record["class_info"]["class"].as_u64().unwrap(),
            "{address:?} class"
        );
        let subsystem = record["id"]["subsystem-vendor"].as_u64().unwrap()
            | (record["id"]["subsystem"].as_u64().unwrap() << 16);
        assert_eq!(
            u64::from(device.config_read(11)),
            subsystem,
            "{address:?} subsystem"
        );
        assert_eq!(
            u64::from((device.config_read(15) >> 8) & 255),
            record["irq_pin"].as_u64().unwrap(),
            "{address:?} interrupt pin"
        );
        let bars = device.bars();
        let regions = record["regions"].as_array().unwrap();
        for region in regions {
            let index = region["bar"].as_u64().unwrap() as u8;
            if address == (2, 0) && index == 6 {
                // The launch has no PXE ROM. OpenCore boots from AHCI instead.
                assert!(!bars.iter().any(|bar| bar.index == 6));
                continue;
            }
            let bar = bars.iter().find(|bar| bar.index == index).unwrap();
            assert_eq!(
                bar.size,
                region["size"].as_u64().unwrap(),
                "{address:?} BAR{index}"
            );
            assert_eq!(
                matches!(bar.kind, BarKind::Io),
                region["type"] == "io",
                "{address:?} BAR{index} kind"
            );
            let reg = if index == 6 {
                12
            } else {
                4 + usize::from(index)
            };
            let flags = root.config_read(0, address.0, address.1, reg);
            if region["type"] == "memory" {
                assert_eq!(
                    flags & 6 == 4,
                    region["mem_type_64"].as_bool().unwrap(),
                    "{address:?} BAR{index} memory width"
                );
                assert_eq!(
                    flags & 8 != 0,
                    region["prefetch"].as_bool().unwrap(),
                    "{address:?} BAR{index} prefetchability"
                );
            } else {
                assert_eq!(flags & 1, 1, "{address:?} BAR{index} I/O flag");
            }
        }
        assert_eq!(
            bars.len(),
            regions.len() - usize::from(address == (2, 0)),
            "{address:?} BAR count"
        );
    }
}
