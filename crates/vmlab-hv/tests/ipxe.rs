//! P1: execute our upstream iPXE ROM under SeaBIOS through the control socket.

#[test]
fn nic_rom_decode_requires_enable_and_memory_space() {
    use parking_lot::Mutex;
    use std::sync::Arc;
    use vmlab_hv_devices::GuestMemory;
    use vmlab_hv_devices::virtio::{
        net::{Net, NetConfig},
        pci::VirtioPciDevice,
    };
    use vmlab_hv_vmm::pci::PciRoot;

    let image =
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/ipxe/virtio-net.rom");
    if !image.is_file() {
        eprintln!("SKIP: run just firmware-ipxe-build for NIC ROM decode test");
        return;
    }
    let bytes = std::fs::read(image).unwrap();
    let net = Net::new(NetConfig {
        id: "net".into(),
        mac: [2, 0, 0, 0, 0, 1],
        mtu: 1500,
        switch: "/nonexistent/vmlab-test-switch".into(),
    })
    .unwrap();
    let mem = GuestMemory::from_ranges(&[(Default::default(), 1 << 20)]).unwrap();
    let pci = VirtioPciDevice::new_transitional("net", Box::new(net), mem, None, None, 4)
        .unwrap()
        .with_option_rom(&bytes)
        .unwrap();
    let mut root = PciRoot::q35(1 << 32);
    root.add_device_at(3, 0, Arc::new(Mutex::new(pci))).unwrap();
    root.config_write(0, 3, 0, 0x30, &u32::MAX.to_le_bytes());
    assert_eq!(root.config_read(0, 3, 0, 12), 0xfffc_0000);
    let mut read = [0; 2];
    root.config_write(0, 3, 0, 0x30, &0xfebc_0000u32.to_le_bytes());
    assert!(!root.dispatch_mmio_read(0xfebc_0000, &mut read));
    root.config_write(0, 3, 0, 0x30, &0xfebc_0001u32.to_le_bytes());
    root.config_write(0, 3, 0, 4, &0u16.to_le_bytes());
    assert!(!root.dispatch_mmio_read(0xfebc_0000, &mut read));
    root.config_write(0, 3, 0, 4, &2u16.to_le_bytes());
    assert!(root.dispatch_mmio_read(0xfebc_0000, &mut read));
    assert_eq!(read, [0x55, 0xaa]);
    assert!(root.dispatch_mmio_write(0xfebc_0000, &[0; 2]));
    assert!(root.dispatch_mmio_read(0xfebc_0000, &mut read));
    assert_eq!(read, [0x55, 0xaa]);
    root.config_write(0, 3, 0, 0x30, &0xfebc_0000u32.to_le_bytes());
    assert!(!root.dispatch_mmio_read(0xfebc_0000, &mut read));
}

#[test]
fn seabios_executes_ipxe_option_rom() {
    let script =
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../firmware/ipxe/boot.py");
    for _ in 0..3 {
        let output = std::process::Command::new("python3")
            .arg(&script)
            .args(["--binary", env!("CARGO_BIN_EXE_vmlab-hv")])
            .output()
            .expect("run iPXE firmware boot probe");
        assert!(
            output.status.success(),
            "{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        eprint!("{}", String::from_utf8_lossy(&output.stdout));
    }
}
